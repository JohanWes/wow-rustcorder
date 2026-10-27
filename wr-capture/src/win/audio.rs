// SPDX-License-Identifier: GPL-3.0-or-later

//! WASAPI capture, mixing and AAC encoding.
//!
//! Each source's samples are placed on the shared clock by their QPC
//! timestamps. Every 10 ms the mixer takes everything up to 50 ms ago from
//! all sources, filling gaps with silence, so a source that is silent, absent
//! (no game running, no microphone) or briefly late never stalls the others,
//! and audio timestamps follow the same clock as video.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IAgileObject, IAgileObject_Impl, STGM_READ,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, HSTRING, IUnknown, Interface, Ref, implement};

use super::encoder::{AUDIO_RATE, Encoder};
use super::{Message, Shared, clock_origin, fail, now_100ns};
use crate::{AudioSource, Options};

const RATE: i64 = AUDIO_RATE as i64;
/// How far the mixer trails real time, so late packets still land.
const LATENCY: i64 = RATE / 20;
/// Timestamp jitter treated as contiguous rather than a gap.
const JITTER: i64 = RATE / 100;
/// How far a source may run ahead of the shared clock before it is pulled
/// back: device clocks drift from QPC by tens of ppm, which adds up over a
/// raid night.
const AHEAD: i64 = RATE / 20;
const RETRY: Duration = Duration::from_secs(2);

pub fn run(options: &Options, shared: &Shared, sender: Sender<Message>) -> Result<(), String> {
    let mut encoder = Encoder::aac()?;
    let origin = clock_origin();
    let mut sources = options.audio.clone();
    if sources.contains(&AudioSource::App) && windows_build() < PROCESS_LOOPBACK_BUILD {
        eprintln!("this Windows cannot capture one app's audio; recording desktop audio");
        sources.retain(|source| *source != AudioSource::App);
        if !sources.contains(&AudioSource::DefaultOutput) {
            sources.insert(0, AudioSource::DefaultOutput);
        }
    }
    let mut tracks: Vec<Track> = sources.into_iter().map(Track::new).collect();
    let mut mixed_until = now_samples() - LATENCY;
    let mut mix = Vec::new();
    let mut pcm = Vec::new();
    let mut packets = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let pid = shared.target_pid.load(Ordering::Relaxed);
        for track in &mut tracks {
            track.service(pid, origin);
        }
        let target = now_samples() - LATENCY;
        if target - mixed_until > RATE {
            // Stalled for over a second: skip ahead rather than catch up.
            mixed_until = target - RATE / 10;
        }
        let frames = target - mixed_until;
        if frames <= 0 {
            continue;
        }
        mix.clear();
        mix.resize(frames as usize * 2, 0f32);
        for track in &mut tracks {
            track.mix_into(mixed_until, &mut mix);
        }
        pcm.clear();
        pcm.extend(
            mix.iter()
                .map(|value| (value.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
        let pts = mixed_until * 10_000_000 / RATE;
        encoder.encode_audio(&pcm, pts, frames * 10_000_000 / RATE, &mut packets)?;
        for packet in packets.drain(..) {
            if sender.send(Message::Packet(packet)).is_err() {
                return Ok(());
            }
        }
        mixed_until = target;
    }
}

/// Process loopback capture arrived in Windows build 20348 (Windows 11).
const PROCESS_LOOPBACK_BUILD: u32 = 20_348;

fn windows_build() -> u32 {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;
    let mut buffer = [0u16; 16];
    let mut size = (buffer.len() * 2) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if read.is_err() {
        return 0;
    }
    let len = buffer.iter().position(|&unit| unit == 0).unwrap_or(0);
    String::from_utf16_lossy(&buffer[..len])
        .parse()
        .unwrap_or(0)
}

fn now_samples() -> i64 {
    now_100ns() * RATE / 10_000_000
}

struct Track {
    source: AudioSource,
    client: Option<Client>,
    /// The process followed by app audio.
    pid: u32,
    retry_at: Instant,
    last_error: String,
    /// Sample index of `samples[0]` on the shared clock.
    start: i64,
    samples: VecDeque<[f32; 2]>,
}

impl Track {
    fn new(source: AudioSource) -> Self {
        Self {
            source,
            client: None,
            pid: 0,
            retry_at: Instant::now(),
            last_error: String::new(),
            start: 0,
            samples: VecDeque::new(),
        }
    }

    fn service(&mut self, pid: u32, origin: i64) {
        if self.source == AudioSource::App && pid != self.pid {
            self.pid = pid;
            self.client = None;
            self.retry_at = Instant::now();
        }
        if self.client.is_none() && Instant::now() >= self.retry_at {
            self.retry_at = Instant::now() + RETRY;
            let opened = match &self.source {
                AudioSource::App if self.pid == 0 => return,
                AudioSource::App => Client::process(self.pid, origin),
                AudioSource::DefaultOutput => Client::default(eRender, origin),
                AudioSource::DefaultInput => Client::default(eCapture, origin),
                AudioSource::Device(id) => Client::device(id, origin),
            };
            match opened {
                Ok(client) => {
                    eprintln!("audio {:?} started", self.source);
                    self.last_error.clear();
                    self.client = Some(client);
                }
                Err(message) => self.report(message),
            }
        }
        let Some(mut client) = self.client.take() else {
            return;
        };
        // A loopback or mic stream keeps running on the old device when the
        // user switches the default one (e.g. plugs in a headset).
        if let Some(flow) = client.default_flow
            && Instant::now() >= self.retry_at
        {
            self.retry_at = Instant::now() + RETRY;
            if default_device_id(flow).is_some_and(|id| id != client.device_id) {
                eprintln!("audio {:?}: the default device changed", self.source);
                self.retry_at = Instant::now();
                return;
            }
        }
        match client.read(|frames, at| self.push(frames, at)) {
            Ok(()) => self.client = Some(client),
            // A changed default device or an exited process ends up here.
            Err(message) => self.report(message),
        }
    }

    fn report(&mut self, message: String) {
        if message != self.last_error {
            eprintln!("audio {:?}: {message}", self.source);
            self.last_error = message;
        }
    }

    /// Append interleaved stereo frames captured at sample index `at`.
    fn push(&mut self, frames: &[f32], at: i64) {
        let end = self.start + self.samples.len() as i64;
        if self.samples.is_empty() {
            self.start = at;
        } else if at > end + JITTER {
            let gap = (at - end).min(RATE) as usize;
            self.samples.extend(std::iter::repeat_n([0.0; 2], gap));
        } else if at < end - AHEAD {
            let back = ((end - at) as usize).min(self.samples.len());
            self.samples.truncate(self.samples.len() - back);
        }
        self.samples
            .extend(frames.chunks_exact(2).map(|pair| [pair[0], pair[1]]));
        // A clock running ahead must not grow the buffer without bound.
        let excess = self.samples.len().saturating_sub(RATE as usize * 2);
        self.samples.drain(..excess);
        self.start += excess as i64;
    }

    /// Add this source's samples for `[from, from + mix.len() / 2)` to `mix`.
    fn mix_into(&mut self, from: i64, mix: &mut [f32]) {
        let stale = (from - self.start).clamp(0, self.samples.len() as i64) as usize;
        self.samples.drain(..stale);
        self.start += stale as i64;
        let offset = (self.start - from).max(0) as usize;
        for index in offset..mix.len() / 2 {
            let Some([left, right]) = self.samples.pop_front() else {
                break;
            };
            mix[index * 2] += left;
            mix[index * 2 + 1] += right;
            self.start += 1;
        }
    }
}

struct Client {
    capture: IAudioCaptureClient,
    _audio: IAudioClient,
    event: Option<HANDLE>,
    origin: i64,
    /// For the default device: its flow and the endpoint it opened.
    default_flow: Option<EDataFlow>,
    device_id: String,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(event) = self.event {
            let _ = unsafe { CloseHandle(event) };
        }
    }
}

impl Client {
    fn default(flow: EDataFlow, origin: i64) -> Result<Self, String> {
        let device = unsafe { enumerator()?.GetDefaultAudioEndpoint(flow, eConsole) }
            .map_err(fail("find the default device"))?;
        let mut client = Self::open(&device, origin)?;
        client.default_flow = Some(flow);
        client.device_id = default_device_id(flow).unwrap_or_default();
        Ok(client)
    }

    fn device(id: &str, origin: i64) -> Result<Self, String> {
        let device = unsafe { enumerator()?.GetDevice(&HSTRING::from(id)) }
            .map_err(fail("find the device"))?;
        Self::open(&device, origin)
    }

    /// Outputs are recorded as loopback, inputs directly.
    fn open(device: &IMMDevice, origin: i64) -> Result<Self, String> {
        let (audio, flow) = unsafe {
            let flow = device
                .cast::<IMMEndpoint>()
                .and_then(|endpoint| endpoint.GetDataFlow())
                .map_err(fail("open the device"))?;
            let audio: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(fail("open the device"))?;
            (audio, flow)
        };
        let flags = if flow == eRender {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        Self::start(audio, flags, None, origin)
    }

    /// Loopback of one process tree (Windows 10 2004 and later).
    fn process(pid: u32, origin: i64) -> Result<Self, String> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // The blob points at `params`; ManuallyDrop keeps PropVariantClear
        // from freeing stack memory.
        let mut variant = ManuallyDrop::new(PROPVARIANT::default());
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob = BLOB {
                cbSize: size_of_val(&params) as u32,
                pBlobData: &params as *const _ as *mut u8,
            };
        }
        let (done, activated) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(done)).into();
        let operation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&*variant),
                &handler,
            )
        }
        .map_err(fail("request process audio"))?;
        activated
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "process audio activation timed out".to_string())?;
        let mut result = HRESULT(0);
        let mut unknown: Option<IUnknown> = None;
        unsafe { operation.GetActivateResult(&mut result, &mut unknown) }
            .map_err(fail("activate process audio"))?;
        result.ok().map_err(fail("activate process audio"))?;
        let audio: IAudioClient = unknown
            .ok_or("no process audio client")?
            .cast()
            .map_err(fail("activate process audio"))?;
        let event =
            unsafe { CreateEventW(None, false, false, None) }.map_err(fail("create an event"))?;
        Self::start(
            audio,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            Some(event),
            origin,
        )
    }

    fn start(
        audio: IAudioClient,
        flags: u32,
        event: Option<HANDLE>,
        origin: i64,
    ) -> Result<Self, String> {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: AUDIO_RATE,
            nAvgBytesPerSec: AUDIO_RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let flags =
            flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let capture = unsafe {
            audio
                // 1 s of slack: opening app audio can block this thread for
                // seconds while the other sources keep filling their buffers.
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    10_000_000,
                    0,
                    &format,
                    None,
                )
                .map_err(fail("initialize audio"))?;
            if let Some(event) = event {
                audio
                    .SetEventHandle(event)
                    .map_err(fail("set the audio event"))?;
            }
            let capture: IAudioCaptureClient =
                audio.GetService().map_err(fail("open audio capture"))?;
            audio.Start().map_err(fail("start audio"))?;
            capture
        };
        Ok(Self {
            capture,
            _audio: audio,
            event,
            origin,
            default_flow: None,
            device_id: String::new(),
        })
    }

    /// Hand every waiting packet to `sink` as (stereo frames, sample index).
    fn read(&mut self, mut sink: impl FnMut(&[f32], i64)) -> Result<(), String> {
        let mut silence = Vec::new();
        loop {
            let waiting =
                unsafe { self.capture.GetNextPacketSize() }.map_err(fail("read audio"))?;
            if waiting == 0 {
                return Ok(());
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut qpc = 0u64;
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
            }
            .map_err(fail("read audio"))?;
            let at_100ns = if qpc != 0 && flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0
            {
                qpc as i64 - self.origin
            } else {
                now_100ns() - i64::from(frames) * 10_000_000 / RATE
            };
            let at = at_100ns * RATE / 10_000_000;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                silence.resize(frames as usize * 2, 0.0);
                sink(&silence, at);
            } else {
                sink(
                    unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * 2) },
                    at,
                );
            }
            unsafe { self.capture.ReleaseBuffer(frames) }.map_err(fail("read audio"))?;
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(fail("list audio devices"))
}

fn device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let id = device.GetId().ok()?;
        let text = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

/// The endpoint id of the current default device for `flow`.
fn default_device_id(flow: EDataFlow) -> Option<String> {
    let device = unsafe { enumerator().ok()?.GetDefaultAudioEndpoint(flow, eConsole) }.ok()?;
    device_id(&device)
}

/// Print `output|<id>|<name>` and `input|<id>|<name>` for every active endpoint.
pub fn list_devices() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let Ok(enumerator) = enumerator() else {
        return;
    };
    for (flow, kind) in [(eRender, "output"), (eCapture, "input")] {
        let Ok(devices) = (unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) })
        else {
            continue;
        };
        for index in 0..unsafe { devices.GetCount() }.unwrap_or(0) {
            let Ok(device) = (unsafe { devices.Item(index) }) else {
                continue;
            };
            let name = unsafe {
                device
                    .OpenPropertyStore(STGM_READ)
                    .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            }
            .map(|value| value.to_string())
            .unwrap_or_default();
            if let Some(id) = device_id(&device) {
                println!("{kind}|{id}|{name}");
            }
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(Mutex<Sender<()>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.lock().map(|done| done.send(()));
        Ok(())
    }
}

impl IAgileObject_Impl for Activated_Impl {}
