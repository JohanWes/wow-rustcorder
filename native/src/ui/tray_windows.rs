// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows notification-area icon. A hidden window on its own thread owns the
//! icon and receives its clicks: left click opens the main window, right click
//! shows Open/Quit. Same interface as the StatusNotifier tray on Linux.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, IMAGE_ICON, LR_SHARED,
    LoadImageW, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_DESTROY, WM_LBUTTONUP,
    WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayEvent {
    Open,
}

const ICON_MESSAGE: u32 = WM_APP + 1;
const TITLE_MESSAGE: u32 = WM_APP + 2;
const MENU_OPEN: usize = 1;
const MENU_QUIT: usize = 2;

/// What the window procedure needs; there is only ever one tray.
struct Shared {
    events: SyncSender<TrayEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    quit_requested: Arc<AtomicBool>,
    title: Mutex<String>,
    /// Broadcast when Explorer restarts; the icon must be added again.
    taskbar_created: u32,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

pub struct TrayBackend {
    hwnd: usize,
    quit_requested: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TrayBackend {
    pub fn start(
        events: SyncSender<TrayEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let quit_requested = Arc::new(AtomicBool::new(false));
        let shared = Shared {
            events,
            wake,
            quit_requested: Arc::clone(&quit_requested),
            title: Mutex::new("Warcraft Rustcorder".into()),
            // SAFETY: a valid NUL-terminated string.
            taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        };
        SHARED
            .set(shared)
            .map_err(|_| "tray started twice".to_owned())?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || run(ready_tx))
            .map_err(|error| error.to_string())?;
        let hwnd = ready_rx
            .recv()
            .map_err(|_| "tray thread exited".to_owned())??;
        Ok(Self {
            hwnd,
            quit_requested,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn is_available(&self) -> bool {
        true
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Acquire)
    }

    pub fn update(&self, title: impl Into<String>) {
        if let Some(shared) = SHARED.get() {
            *shared.title.lock().expect("tray title") = title.into();
        }
        // SAFETY: posting to a window owned by the tray thread; harmless if
        // it is already gone.
        unsafe { PostMessageW(self.hwnd as HWND, TITLE_MESSAGE, 0, 0) };
    }

    pub fn shutdown(&self) {
        // SAFETY: as in `update`.
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        if let Some(thread) = self.thread.lock().expect("tray thread").take() {
            let _ = thread.join();
        }
    }
}

fn run(ready: mpsc::Sender<Result<usize, String>>) {
    // SAFETY: plain Win32 window and message-loop calls on this thread.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("WarcraftRustcorderTray");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&window_class);
        // A hidden top-level window rather than a message-only one, which
        // would miss the TaskbarCreated broadcast.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err("cannot create the tray window".into()));
            return;
        }
        if Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)) == 0 {
            DestroyWindow(hwnd);
            let _ = ready.send(Err("the notification area refused the icon".into()));
            return;
        }
        let _ = ready.send(Ok(hwnd as usize));
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(shared) = SHARED.get() else {
        // SAFETY: default handling for the window this procedure belongs to.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    // SAFETY: every call below targets this thread's own window and menu.
    unsafe {
        match message {
            ICON_MESSAGE => {
                match lparam as u32 {
                    WM_LBUTTONUP => shared.open(),
                    WM_RBUTTONUP => show_menu(hwnd, shared),
                    _ => {}
                }
                0
            }
            TITLE_MESSAGE => {
                Shell_NotifyIconW(NIM_MODIFY, &icon_data(hwnd));
                0
            }
            WM_DESTROY => {
                Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                PostQuitMessage(0);
                0
            }
            _ if message == shared.taskbar_created => {
                Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

impl Shared {
    /// Non-blocking: Open is idempotent, so a dropped one under a full
    /// channel is harmless.
    fn open(&self) {
        let _ = self.events.try_send(TrayEvent::Open);
        (self.wake)();
    }

    fn quit(&self) {
        self.quit_requested.store(true, Ordering::Release);
        (self.wake)();
    }
}

/// # Safety
/// `hwnd` is the tray window, called on its thread.
unsafe fn show_menu(hwnd: HWND, shared: &Shared) {
    // SAFETY: per the function contract.
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_OPEN, wide("Open").as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, wide("Quit").as_ptr());
        let mut cursor = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cursor);
        // Without this the menu does not close when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            0,
            hwnd,
            std::ptr::null(),
        ) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen {
            MENU_OPEN => shared.open(),
            MENU_QUIT => shared.quit(),
            _ => {}
        }
    }
}

/// The icon is the exe's own (resource 1, embedded by build.rs).
fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: ICON_MESSAGE,
        ..Default::default()
    };
    // SAFETY: loads a shared icon from this module's resources.
    data.hIcon = unsafe {
        LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1 as _,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
    };
    if let Some(shared) = SHARED.get() {
        let title = shared.title.lock().expect("tray title");
        for (slot, unit) in data.szTip[..127].iter_mut().zip(title.encode_utf16()) {
            *slot = unit;
        }
    }
    data
}

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
