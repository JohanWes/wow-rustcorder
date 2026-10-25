// SPDX-License-Identifier: GPL-3.0-or-later

//! Media Foundation encoder transforms. Hardware video encoders are
//! asynchronous (they announce when they want input and when output is
//! ready); the software H.264 and AAC encoders are synchronous. `encode`
//! hides the difference and returns whatever packets are ready.

use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::{GUID, Interface, PWSTR};

use super::convert::Frame;
use super::fail;
use crate::packet::{Packet, VideoCodec};

pub const AUDIO_RATE: u32 = 48_000;
const AUDIO_BYTES_PER_SECOND: u32 = 20_000; // 160 kbps

pub struct Encoder {
    transform: IMFTransform,
    /// Present for asynchronous transforms.
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    need_input: u32,
    provides_samples: bool,
    output_size: u32,
    /// `None` for audio.
    codec: Option<VideoCodec>,
    parameter_sets: Option<Vec<u8>>,
    pub name: String,
    _manager: Option<IMFDXGIDeviceManager>,
}

impl Encoder {
    /// The first hardware encoder on the capture GPU that accepts the
    /// settings, then the software encoder.
    pub fn video(
        device: &ID3D11Device,
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
    ) -> Result<Self, String> {
        let subtype = match codec {
            VideoCodec::H264 => MFVideoFormat_H264,
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            VideoCodec::Av1 => MFVideoFormat_AV1,
        };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let luid = device
            .cast::<IDXGIDevice>()
            .and_then(|dxgi| unsafe { dxgi.GetAdapter() })
            .and_then(|adapter| unsafe { adapter.GetDesc() })
            .map_err(fail("read the GPU adapter"))?
            .AdapterLuid;
        let attributes = attributes().map_err(fail("create attributes"))?;
        let luid_bytes = unsafe {
            std::slice::from_raw_parts(&luid as *const _ as *const u8, size_of_val(&luid))
        };
        unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes) }
            .map_err(fail("set the adapter"))?;

        let hardware = enumerate(
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            &input,
            &output,
            Some(&attributes),
        );
        let software = enumerate(
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            &input,
            &output,
            None,
        );
        let candidates = hardware
            .into_iter()
            .map(|activate| (activate, true))
            .chain(software.into_iter().map(|activate| (activate, false)));
        for (activate, is_hardware) in candidates {
            let name = friendly_name(&activate);
            match Self::open_video(
                &activate,
                is_hardware,
                device,
                codec,
                subtype,
                width,
                height,
                fps,
                kbps,
            ) {
                Ok(mut encoder) => {
                    encoder.name = name;
                    return Ok(encoder);
                }
                Err(message) => eprintln!("encoder {name} unusable: {message}"),
            }
        }
        Err(format!("no usable {codec:?} encoder"))
    }

    #[allow(clippy::too_many_arguments)]
    fn open_video(
        activate: &IMFActivate,
        is_hardware: bool,
        device: &ID3D11Device,
        codec: VideoCodec,
        subtype: GUID,
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
    ) -> Result<Self, String> {
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(fail("activate"))?;
        let is_async = unlock_async(&transform)?;
        let manager = if is_hardware {
            let mut token = 0;
            let mut manager = None;
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
                .map_err(fail("create the device manager"))?;
            let manager = manager.ok_or("no device manager")?;
            unsafe {
                manager
                    .ResetDevice(device, token)
                    .map_err(fail("share the device"))?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .map_err(fail("set the device manager"))?;
            }
            Some(manager)
        } else {
            None
        };
        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            // Unsupported settings are skipped; encoders differ here.
            let set = |key: &GUID, value: u32| unsafe {
                let _ = api.SetValue(key, &variant(value));
            };
            set(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
            );
            set(&CODECAPI_AVEncCommonMeanBitRate, kbps * 1000);
            set(&CODECAPI_AVEncMPVGOPSize, fps * 2);
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
            set(&CODECAPI_AVLowLatencyMode, 1);
        }
        let size = (u64::from(width) << 32) | u64::from(height);
        let rate = (u64::from(fps) << 32) | 1;
        unsafe {
            let output = MFCreateMediaType().map_err(fail("create the output type"))?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(fail("output type"))?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &subtype)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)
                .map_err(fail("output type"))?;
            output
                .SetUINT64(&MF_MT_FRAME_SIZE, size)
                .map_err(fail("output type"))?;
            output
                .SetUINT64(&MF_MT_FRAME_RATE, rate)
                .map_err(fail("output type"))?;
            output
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(fail("output type"))?;
            let profile = match codec {
                VideoCodec::H264 => eAVEncH264VProfile_High.0 as u32,
                VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
                VideoCodec::Av1 => eAVEncAV1VProfile_Main_420_8.0 as u32,
            };
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
                .map_err(fail("output type"))?;
            output
                .SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
                .map_err(fail("output type"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(fail("set the output type"))?;

            let input = MFCreateMediaType().map_err(fail("create the input type"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(fail("input type"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(fail("input type"))?;
            input
                .SetUINT64(&MF_MT_FRAME_SIZE, size)
                .map_err(fail("input type"))?;
            input
                .SetUINT64(&MF_MT_FRAME_RATE, rate)
                .map_err(fail("input type"))?;
            input
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
                .map_err(fail("input type"))?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(fail("input type"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(fail("set the input type"))?;
        }
        Self::begin(transform, is_async, codec_api, Some(codec), manager)
    }

    /// The system AAC encoder, emitting ADTS frames.
    pub fn aac() -> Result<Self, String> {
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER) }
                .map_err(fail("create the AAC encoder"))?;
        unsafe {
            let output = MFCreateMediaType().map_err(fail("create the AAC type"))?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(fail("AAC type"))?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(fail("AAC type"))?;
            output
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(fail("AAC type"))?;
            output
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)
                .map_err(fail("AAC type"))?;
            output
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
                .map_err(fail("AAC type"))?;
            output
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BYTES_PER_SECOND)
                .map_err(fail("AAC type"))?;
            output
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 1)
                .map_err(fail("AAC type"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(fail("set the AAC type"))?;

            let input = MFCreateMediaType().map_err(fail("create the PCM type"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(fail("PCM type"))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(fail("PCM type"))?;
            input
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(fail("PCM type"))?;
            input
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)
                .map_err(fail("PCM type"))?;
            input
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
                .map_err(fail("PCM type"))?;
            input
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
                .map_err(fail("PCM type"))?;
            input
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 4)
                .map_err(fail("PCM type"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(fail("set the PCM type"))?;
        }
        let mut encoder = Self::begin(transform, false, None, None, None)?;
        encoder.name = "AAC".into();
        Ok(encoder)
    }

    fn begin(
        transform: IMFTransform,
        is_async: bool,
        codec_api: Option<ICodecAPI>,
        codec: Option<VideoCodec>,
        manager: Option<IMFDXGIDeviceManager>,
    ) -> Result<Self, String> {
        let info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(fail("read the output stream info"))?;
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(fail("begin streaming"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(fail("start the stream"))?;
        }
        let events = if is_async {
            Some(
                transform
                    .cast()
                    .map_err(fail("query the event generator"))?,
            )
        } else {
            None
        };
        Ok(Self {
            transform,
            events,
            codec_api,
            need_input: 0,
            provides_samples: info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0,
            output_size: info.cbSize,
            codec,
            parameter_sets: None,
            name: String::new(),
            _manager: manager,
        })
    }

    pub fn encode_video(
        &mut self,
        frame: &Frame,
        pts: i64,
        duration: i64,
        keyframe: bool,
        out: &mut Vec<Packet>,
    ) -> Result<(), String> {
        let sample = unsafe {
            let buffer = match frame {
                Frame::Texture(texture) => {
                    let buffer =
                        MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
                            .map_err(fail("wrap the frame"))?;
                    let length = buffer
                        .cast::<IMF2DBuffer>()
                        .and_then(|buffer| buffer.GetContiguousLength())
                        .map_err(fail("size the frame"))?;
                    buffer
                        .SetCurrentLength(length)
                        .map_err(fail("size the frame"))?;
                    buffer
                }
                Frame::Memory(bytes) => memory_buffer(bytes)?,
            };
            sample(&buffer, pts, duration)?
        };
        if keyframe && let Some(api) = &self.codec_api {
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant(1)) };
        }
        self.encode(&sample, out)
    }

    /// Interleaved 16-bit stereo PCM.
    /// Interleaved 16-bit stereo PCM.
    pub fn encode_audio(
        &mut self,
        pcm: &[i16],
        pts: i64,
        duration: i64,
        out: &mut Vec<Packet>,
    ) -> Result<(), String> {
        let bytes = unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * 2) };
        let sample = unsafe { sample(&memory_buffer(bytes)?, pts, duration)? };
        self.encode(&sample, out)
    }

    fn encode(&mut self, sample: &IMFSample, out: &mut Vec<Packet>) -> Result<(), String> {
        let Some(events) = self.events.clone() else {
            unsafe { self.transform.ProcessInput(0, sample, 0) }.map_err(fail("encode"))?;
            while self.output(out)? {}
            return Ok(());
        };
        while self.need_input == 0 {
            let event = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }
                .map_err(fail("wait for the encoder"))?;
            self.handle(&event, out)?;
        }
        unsafe { self.transform.ProcessInput(0, sample, 0) }.map_err(fail("encode"))?;
        self.need_input -= 1;
        loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle(&event, out)?,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(format!("read encoder events: {error}")),
            }
        }
    }

    fn handle(&mut self, event: &IMFMediaEvent, out: &mut Vec<Packet>) -> Result<(), String> {
        let kind = unsafe { event.GetType() }.map_err(fail("read an encoder event"))?;
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.output(out)?;
        }
        Ok(())
    }

    /// Fetch one output; false when the encoder needs more input.
    fn output(&mut self, out: &mut Vec<Packet>) -> Result<bool, String> {
        let sample = if self.provides_samples {
            None
        } else {
            unsafe {
                let buffer = MFCreateMemoryBuffer(self.output_size.max(1 << 16))
                    .map_err(fail("allocate output"))?;
                let sample = MFCreateSample().map_err(fail("allocate output"))?;
                sample.AddBuffer(&buffer).map_err(fail("allocate output"))?;
                Some(sample)
            }
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // Some hardware encoders settle their output type only now.
                unsafe {
                    let output = self
                        .transform
                        .GetOutputAvailableType(0, 0)
                        .map_err(fail("renegotiate output"))?;
                    self.transform
                        .SetOutputType(0, &output, 0)
                        .map_err(fail("renegotiate output"))?;
                    let info = self
                        .transform
                        .GetOutputStreamInfo(0)
                        .map_err(fail("renegotiate output"))?;
                    self.output_size = info.cbSize;
                }
                self.parameter_sets = None;
                Ok(true)
            }
            Err(error) => Err(format!("read encoder output: {error}")),
            Ok(()) => {
                if let Some(sample) = sample {
                    out.push(self.packet(&sample)?);
                }
                Ok(true)
            }
        }
    }

    fn packet(&mut self, sample: &IMFSample) -> Result<Packet, String> {
        let (pts, key, mut data) = unsafe {
            let pts = sample
                .GetSampleTime()
                .map_err(fail("read the packet time"))?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(fail("read the packet"))?;
            let mut pointer = std::ptr::null_mut();
            let mut length = 0;
            buffer
                .Lock(&mut pointer, None, Some(&mut length))
                .map_err(fail("read the packet"))?;
            let data = std::slice::from_raw_parts(pointer, length as usize).to_vec();
            let _ = buffer.Unlock();
            (pts, key, data)
        };
        let Some(codec) = self.codec else {
            return Ok(Packet {
                video: false,
                key: true,
                pts,
                data,
            });
        };
        // Readers need the parameter sets (AV1: sequence header) in-band on
        // every keyframe.
        if key && !has_parameter_sets(&data, codec) {
            if self.parameter_sets.is_none() {
                self.parameter_sets = unsafe { self.transform.GetOutputCurrentType(0) }
                    .ok()
                    .and_then(|output| sequence_header(&output));
            }
            if let Some(sets) = &self.parameter_sets {
                data.splice(0..0, sets.iter().copied());
            }
        }
        Ok(Packet {
            video: true,
            key,
            pts,
            data,
        })
    }
}

fn memory_buffer(bytes: &[u8]) -> Result<IMFMediaBuffer, String> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32).map_err(fail("allocate a buffer"))?;
        let mut data = std::ptr::null_mut();
        buffer
            .Lock(&mut data, None, None)
            .map_err(fail("lock a buffer"))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock().map_err(fail("unlock a buffer"))?;
        buffer
            .SetCurrentLength(bytes.len() as u32)
            .map_err(fail("size a buffer"))?;
        Ok(buffer)
    }
}

fn variant(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    variant
}

unsafe fn sample(buffer: &IMFMediaBuffer, pts: i64, duration: i64) -> Result<IMFSample, String> {
    unsafe {
        let sample = MFCreateSample().map_err(fail("create a sample"))?;
        sample.AddBuffer(buffer).map_err(fail("create a sample"))?;
        sample.SetSampleTime(pts).map_err(fail("create a sample"))?;
        sample
            .SetSampleDuration(duration)
            .map_err(fail("create a sample"))?;
        Ok(sample)
    }
}

fn attributes() -> windows::core::Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 1)? };
    attributes.ok_or_else(windows::core::Error::empty)
}

/// Hardware encoders refuse every call until explicitly unlocked.
fn unlock_async(transform: &IMFTransform) -> Result<bool, String> {
    let Ok(attributes) = (unsafe { transform.GetAttributes() }) else {
        return Ok(false);
    };
    let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
    if is_async {
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(fail("unlock the encoder"))?;
    }
    Ok(is_async)
}

fn enumerate(
    flags: MFT_ENUM_FLAG,
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
    attributes: Option<&IMFAttributes>,
) -> Vec<IMFActivate> {
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    let found = unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(input),
            Some(output),
            attributes,
            &mut list,
            &mut count,
        )
    };
    if found.is_err() || list.is_null() {
        return Vec::new();
    }
    let activates = unsafe { std::slice::from_raw_parts_mut(list, count as usize) }
        .iter_mut()
        .filter_map(Option::take)
        .collect();
    unsafe { CoTaskMemFree(Some(list.cast())) };
    activates
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut length = 0;
    if unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut length) }
        .is_err()
    {
        return "unnamed encoder".into();
    }
    let text = unsafe { name.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(name.0.cast())) };
    text
}

fn sequence_header(output: &IMFMediaType) -> Option<Vec<u8>> {
    let size = unsafe { output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    let mut blob = vec![0u8; size as usize];
    unsafe { output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) }.ok()?;
    // AV1 encoders may hand over an `av1C` record; keep only its OBUs.
    if blob.len() > 4 && blob[0] == 0x81 {
        blob.drain(..4);
    }
    Some(blob)
}

/// Whether an access unit carries an SPS (H.264), VPS (HEVC) or sequence
/// header OBU (AV1).
fn has_parameter_sets(data: &[u8], codec: VideoCodec) -> bool {
    if codec == VideoCodec::Av1 {
        return crate::mkv::has_sequence_header(data);
    }
    // Parameter sets lead the access unit.
    data[..data.len().min(512)].windows(4).any(|window| {
        window[..3] == [0, 0, 1]
            && match codec {
                VideoCodec::H264 => window[3] & 0x1F == 7,
                VideoCodec::Hevc => (window[3] >> 1) & 0x3F == 32,
                VideoCodec::Av1 => false,
            }
    })
}
