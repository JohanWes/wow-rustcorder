// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows Graphics Capture of the target and the constant-frame-rate encode
//! loop. Every tick encodes the newest captured frame (or black while the
//! target is missing), so the stream never stalls and timestamps stay exact.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, LPARAM, POINT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
};
use windows::core::{BOOL, Interface, PWSTR};

use super::convert::Converter;
use super::encoder::Encoder;
use super::{Message, Shared, fail, now_100ns};
use crate::{Options, Target};

/// How often a missing target window is looked for.
const SEARCH_INTERVAL: Duration = Duration::from_secs(2);

pub fn run(
    options: &Options,
    shared: &Shared,
    sender: Sender<Message>,
    ready: Sender<Result<(), String>>,
) -> Result<(), String> {
    let setup = || -> Result<_, String> {
        let (device, context) = create_device()?;
        let (width, height) = options.size.unwrap_or_else(primary_monitor_size);
        let converter = Converter::new(&device, &context, width, height, options.fps)?;
        let encoder = Encoder::video(
            &device,
            options.codec,
            width,
            height,
            options.fps,
            options.bitrate_kbps,
        )?;
        let capture = Capture::new(&device, options)?;
        eprintln!(
            "encoding {width}x{height} at {} fps with {}",
            options.fps, encoder.name
        );
        Ok((context, converter, encoder, capture))
    };
    let (context, mut converter, mut encoder, mut capture) = match setup() {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(message) => {
            let _ = ready.send(Err(message));
            return Ok(());
        }
    };

    let fps = i64::from(options.fps);
    let start = now_100ns();
    let mut index: i64 = 0;
    let mut packets = Vec::new();
    loop {
        let mut due = start + index * 10_000_000 / fps;
        let now = now_100ns();
        if due > now {
            std::thread::sleep(Duration::from_nanos((due - now) as u64 * 100));
        } else if now - due > 20_000_000 / fps {
            // Fell more than two frames behind: drop the missed ticks.
            index = (now - start) * fps / 10_000_000;
            due = start + index * 10_000_000 / fps;
        }
        capture.poll(&context, shared);
        let frame = converter.convert(capture.latest.as_ref())?;
        let keyframe = shared.keyframe.swap(false, Ordering::Relaxed);
        encoder.encode_video(&frame, due, 10_000_000 / fps, keyframe, &mut packets)?;
        for packet in packets.drain(..) {
            if sender.send(Message::Packet(packet)).is_err() {
                return Ok(());
            }
        }
        index += 1;
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(fail("create the Direct3D 11 device"))?;
    }
    let (device, context) = (
        device.ok_or("no Direct3D device")?,
        context.ok_or("no Direct3D context")?,
    );
    // The encoder's Media Foundation threads use the same device.
    let multithread: ID3D11Multithread = context.cast().map_err(fail("query ID3D11Multithread"))?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok((device, context))
}

fn primary_monitor_size() -> (u32, u32) {
    unsafe {
        let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let rect = info.rcMonitor;
            return (
                ((rect.right - rect.left) as u32) & !1,
                ((rect.bottom - rect.top) as u32) & !1,
            );
        }
    }
    (1920, 1080)
}

struct Active {
    /// The captured window, `None` for the monitor.
    window: Option<HWND>,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
}

impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

struct Capture {
    device: ID3D11Device,
    winrt_device: IDirect3DDevice,
    target: Target,
    cursor: bool,
    active: Option<Active>,
    next_search: Instant,
    /// Copy of the newest frame with its content size.
    latest: Option<(ID3D11Texture2D, u32, u32)>,
}

impl Capture {
    fn new(device: &ID3D11Device, options: &Options) -> Result<Self, String> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("Windows Graphics Capture is not supported on this system".into());
        }
        let dxgi: IDXGIDevice = device.cast().map_err(fail("query IDXGIDevice"))?;
        let winrt_device = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|inspectable| inspectable.cast())
            .map_err(fail("wrap the Direct3D device for capture"))?;
        Ok(Self {
            device: device.clone(),
            winrt_device,
            target: options.target.clone(),
            cursor: options.cursor,
            active: None,
            next_search: Instant::now(),
            latest: None,
        })
    }

    /// Follow the target and copy its newest frame, if any arrived.
    fn poll(&mut self, context: &ID3D11DeviceContext, shared: &Shared) {
        if let Some(window) = self.active.as_ref().and_then(|active| active.window)
            && !unsafe { IsWindow(Some(window)) }.as_bool()
        {
            eprintln!("capture target closed");
            self.stop(shared);
        }
        if self.active.is_none() && Instant::now() >= self.next_search {
            self.next_search = Instant::now() + SEARCH_INTERVAL;
            match self.start() {
                Ok(Some(active)) => {
                    let mut pid = 0;
                    if let Some(window) = active.window {
                        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
                    }
                    shared.target_pid.store(pid, Ordering::Relaxed);
                    eprintln!(
                        "capturing {:?} ({}x{}, pid {pid})",
                        self.target, active.size.Width, active.size.Height
                    );
                    self.active = Some(active);
                }
                Ok(None) => {}
                Err(message) => eprintln!("capture start failed: {message}"),
            }
        }
        if let Err(message) = self.copy_newest(context) {
            eprintln!("capture failed: {message}");
            self.stop(shared);
        }
    }

    fn stop(&mut self, shared: &Shared) {
        self.active = None;
        self.latest = None;
        shared.target_pid.store(0, Ordering::Relaxed);
    }

    fn start(&self) -> Result<Option<Active>, String> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(fail("get the capture item factory"))?;
        let (item, window): (GraphicsCaptureItem, _) = match &self.target {
            Target::Exe(exe) => {
                let Some(window) = find_window(exe) else {
                    return Ok(None);
                };
                let item = unsafe { interop.CreateForWindow(window) }
                    .map_err(fail("capture the window"))?;
                (item, Some(window))
            }
            Target::PrimaryMonitor => {
                let monitor =
                    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
                let item = unsafe { interop.CreateForMonitor(monitor) }
                    .map_err(fail("capture the monitor"))?;
                (item, None)
            }
        };
        let size = item.Size().map_err(fail("read the capture size"))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &self.winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(fail("create the frame pool"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(fail("create the capture session"))?;
        // Each is missing on older Windows builds; capture works without them.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(self.cursor);
        let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 });
        session.StartCapture().map_err(fail("start capturing"))?;
        Ok(Some(Active {
            window,
            pool,
            session,
            size,
        }))
    }

    fn copy_newest(&mut self, context: &ID3D11DeviceContext) -> Result<(), String> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let mut newest = None;
        while let Ok(frame) = active.pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        let Some(frame) = newest else {
            return Ok(());
        };
        let size = frame.ContentSize().map_err(fail("read the frame size"))?;
        let texture: ID3D11Texture2D = frame
            .Surface()
            .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
            .and_then(|access| unsafe { access.GetInterface() })
            .map_err(fail("read the frame texture"))?;
        if size != active.size {
            // The window was resized; frames keep arriving at the old size
            // until the pool matches.
            active
                .pool
                .Recreate(
                    &self.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )
                .map_err(fail("resize the frame pool"))?;
            active.size = size;
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let width = (size.Width.max(0) as u32).min(desc.Width);
        let height = (size.Height.max(0) as u32).min(desc.Height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        if self
            .latest
            .as_ref()
            .is_none_or(|(_, w, h)| (*w, *h) != (width, height))
        {
            self.latest = Some((create_bgra(&self.device, width, height)?, width, height));
        }
        let (latest, _, _) = self.latest.as_ref().expect("set above");
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        unsafe { context.CopySubresourceRegion(latest, 0, 0, 0, 0, &texture, 0, Some(&region)) };
        Ok(())
    }
}

pub fn create_bgra(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(fail("create a frame texture"))?;
    texture.ok_or_else(|| "no frame texture".into())
}

/// The first visible, unowned top-level window whose process image is `exe`.
fn find_window(exe: &str) -> Option<HWND> {
    struct Search<'a> {
        exe: &'a str,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, lparam: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let owned = unsafe { GetWindow(window, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid());
        if !unsafe { IsWindowVisible(window) }.as_bool() || owned {
            return true.into();
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if process_name(pid).is_some_and(|name| name.eq_ignore_ascii_case(search.exe)) {
            search.found = Some(window);
            return false.into();
        }
        true.into()
    }
    let mut search = Search { exe, found: None };
    // Stopping early reports an error; the result is in `search`.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let named = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        named.ok()?;
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        path.rsplit('\\').next().map(str::to_string)
    }
}
