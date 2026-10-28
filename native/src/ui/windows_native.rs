// SPDX-License-Identifier: GPL-3.0-or-later

//! Window snapping and font size for the Windows build.
//!
//! GTK draws its own title bar on Windows and gives the window no native
//! frame, so Windows offers none of its snapping. Restoring the frame styles
//! is not an option: GTK 4.24 then miscounts the frame and keeps resizing and
//! moving the window. Instead the empty parts of the header bars report as
//! the caption, so dragging them runs Windows' own move loop, and dropping the
//! window at the top edge maximizes it, at a side edge fills that half of the
//! screen. Dragging a half-screen window away restores its size.
//!
//! The same missing frame styles leave Windows' Win+arrow shortcuts and the
//! system menu without effect, so a keyboard hook, installed only while the
//! window is active, takes Win+Up/Down/Left/Right and does what Windows would,
//! and Alt+Space and a right click on the header open GTK's system menu.
//!
//! GTK also takes Windows' 9 pt message font, which reads small next to the
//! 11 pt the layout was designed with, so the font is raised to that.
//!
//! With DirectComposition on (see `enable_gpu_rendering` in main.rs), GTK's
//! GL renderer draws transparent pixels black. So the window is clipped to its
//! visible part with a window region, which cuts off GTK's invisible resize
//! margin: resizing grabs the visible edge instead. Shadows and rounded
//! corners, which would show as black, are styled away, and popovers lose
//! their arrow, whose strip beside the arrow would show black too.

use std::cell::{Cell, RefCell};

use gtk4::glib::object::ObjectType;
use gtk4::prelude::*;
use libadwaita as adw;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateRectRgn, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    SetWindowRgn,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, KEYEVENTF_KEYUP, VK_CONTROL, VK_DOWN, VK_LEFT, VK_LWIN, VK_MENU, VK_RIGHT,
    VK_RWIN, VK_SHIFT, VK_UP, keybd_event,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetWindowRect, HHOOK, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT,
    HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsIconic, IsZoomed,
    KBDLLHOOKSTRUCT, PostMessageW, SC_KEYMENU, SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SendMessageW, SetWindowPos, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx,
    WA_INACTIVE, WH_KEYBOARD_LL, WM_ACTIVATE, WM_APP, WM_DESTROY, WM_ENTERSIZEMOVE,
    WM_EXITSIZEMOVE, WM_KEYDOWN, WM_MOVING, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCRBUTTONUP,
    WM_SYSCOMMAND, WM_SYSKEYDOWN, WM_WINDOWPOSCHANGED,
};

const MIN_FONT_POINTS: i32 = 11;
/// GTK's resize margin around a window without a shadow (RESIZE_HANDLE_SIZE).
const MARGIN: i32 = 12;
/// How far into the visible edge a resize grab reaches.
const GRIP: i32 = 4;
/// Posted by the keyboard hook with the arrow's virtual key.
const WM_WIN_ARROW: u32 = WM_APP + 1;
/// GTK's handler for it shows the system menu with the frame styles it needs.
const WM_SYSMENU: u32 = 0x313;
/// An unassigned virtual key: pressing it while Win is held stops Windows
/// opening the Start menu when Win is released after a swallowed arrow.
const VK_MASK: u8 = 0xE8;

const CSS: &str = "
window.csd { box-shadow: none; border-radius: 0; }
popover > contents, popover > arrow { box-shadow: none; border-radius: 0; }
tooltip { box-shadow: none; border-radius: 0; }
";

unsafe extern "C" {
    fn gdk_win32_surface_get_handle(surface: *mut gtk4::gdk::ffi::GdkSurface) -> HWND;
}

#[derive(Default)]
struct Drag {
    window: RefCell<Option<gtk4::glib::WeakRef<gtk4::Window>>>,
    /// Window size to return to when a half-screen window is dragged away.
    restore_size: Cell<Option<(i32, i32)>>,
    moved: Cell<bool>,
    /// Horizontal shift keeping the cursor over the same spot of a window
    /// that shrank back to its restore size at the start of this drag.
    shift: Cell<Option<i32>>,
    /// Size and margin of the window region last set.
    region: Cell<(i32, i32, i32)>,
    /// Where a half-screen window came from, for Win+Down.
    restore_origin: Cell<Option<(i32, i32)>>,
    tiled: Cell<Option<Side>>,
    hwnd: Cell<HWND>,
    hook: Cell<HHOOK>,
}

thread_local! {
    static DRAG: Drag = Drag::default();
}

pub fn setup(window: &adw::ApplicationWindow) {
    raise_font_size();
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(CSS);
    gtk4::style_context_add_provider_for_display(
        &WidgetExt::display(window),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    drop_popover_arrows();
    window.connect_realize(|window| {
        let Some(surface) = window.surface() else {
            return;
        };
        DRAG.with(|drag| {
            *drag.window.borrow_mut() = Some(window.upcast_ref::<gtk4::Window>().downgrade());
        });
        // SAFETY: the realized toplevel's HWND, subclassed on the GTK thread
        // that owns it.
        unsafe {
            let hwnd = gdk_win32_surface_get_handle(surface.as_ptr());
            DRAG.with(|drag| drag.hwnd.set(hwnd));
            SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
            clip_to_content(hwnd);
        }
    });
    // GTK forgets where a hidden window was: put it back when shown from
    // the tray, and place the first one, once GTK has given it a size.
    let hidden_at = std::rc::Rc::new(Cell::new(None::<RECT>));
    let saved = std::rc::Rc::clone(&hidden_at);
    window.connect_unmap(move |_| {
        let hwnd = DRAG.with(|drag| drag.hwnd.get());
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: our own window, and an out-parameter.
        unsafe {
            if IsIconic(hwnd) == 0 && IsZoomed(hwnd) == 0 && GetWindowRect(hwnd, &mut frame) != 0 {
                saved.set(Some(frame));
            }
        }
    });
    let placed = Cell::new(false);
    window.connect_map(move |_| {
        let first = !placed.replace(true);
        let frame = hidden_at.take();
        gtk4::glib::idle_add_local_once(move || {
            let hwnd = DRAG.with(|drag| drag.hwnd.get());
            match frame {
                _ if first => place_on_screen(hwnd),
                // SAFETY: moving our own window.
                Some(frame) if unsafe { IsZoomed(hwnd) } == 0 => unsafe {
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        frame.left,
                        frame.top,
                        frame.right - frame.left,
                        frame.bottom - frame.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                },
                _ => {}
            }
        });
    });
}

/// Un-minimize a window hidden to the tray while minimized. GTK would show it
/// minimized again, and minimize-to-tray would then hide it straight away.
pub fn restore_minimized(window: &impl IsA<gtk4::Window>) {
    let Some(surface) = window.as_ref().surface() else {
        return;
    };
    // SAFETY: our own toplevel's HWND.
    unsafe {
        let hwnd = gdk_win32_surface_get_handle(surface.as_ptr());
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }
}

/// GTK's margin in physical pixels; none when maximized or fullscreen.
fn margin(hwnd: HWND) -> i32 {
    let mut margin = 0;
    with_window(|window| {
        let state = window
            .surface()
            .and_downcast::<gtk4::gdk::Toplevel>()
            .map_or(gtk4::gdk::ToplevelState::empty(), |toplevel| {
                toplevel.state()
            });
        let filled = gtk4::gdk::ToplevelState::MAXIMIZED | gtk4::gdk::ToplevelState::FULLSCREEN;
        // SAFETY: our own HWND.
        if !state.intersects(filled) && unsafe { IsZoomed(hwnd) } == 0 {
            margin = MARGIN * window.scale_factor();
        }
    });
    margin
}

/// # Safety
/// `hwnd` is the main window's.
unsafe fn clip_to_content(hwnd: HWND) {
    let margin = margin(hwnd);
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: per the contract; Windows owns the region once set.
    unsafe {
        GetWindowRect(hwnd, &mut frame);
        let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
        // Setting a region sends WM_WINDOWPOSCHANGED again, and GTK answers
        // with a new layout: only set it when it changes, never minimized.
        let changed = DRAG.with(|drag| drag.region.replace((width, height, margin)))
            != (width, height, margin);
        if !changed || IsIconic(hwnd) != 0 {
            return;
        }
        let region = CreateRectRgn(margin, margin, width - margin, height - margin);
        SetWindowRgn(hwnd, region, 1);
    }
}

/// Resize grips just inside the visible edges.
fn edge_hit(hwnd: HWND, point: POINT) -> Option<u32> {
    let margin = margin(hwnd);
    if margin == 0 {
        return None;
    }
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: an out-parameter.
    unsafe { GetWindowRect(hwnd, &mut frame) };
    let grip = margin / MARGIN * GRIP + margin;
    let left = point.x < frame.left + grip;
    let right = point.x >= frame.right - grip;
    let top = point.y < frame.top + grip;
    let bottom = point.y >= frame.bottom - grip;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, ..) => HTLEFT,
        (_, true, ..) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (.., true) => HTBOTTOM,
        _ => return None,
    })
}

/// Turns off the arrow of every popover just before it shows, including the
/// ones GTK and libadwaita build internally (dropdowns, menus).
fn drop_popover_arrows() {
    use gtk4::glib::gobject_ffi;
    use gtk4::glib::translate::{FromGlibPtrNone, IntoGlib};

    unsafe extern "C" fn hook(
        _hint: *mut gobject_ffi::GSignalInvocationHint,
        n_values: u32,
        values: *const gobject_ffi::GValue,
        _data: gtk4::glib::ffi::gpointer,
    ) -> gtk4::glib::ffi::gboolean {
        if n_values > 0 {
            // SAFETY: values[0] is the emitting instance, a GtkWidget.
            let instance = unsafe {
                gtk4::glib::Object::from_glib_none(gobject_ffi::g_value_get_object(values))
            };
            if let Some(popover) = instance.downcast_ref::<gtk4::Popover>() {
                popover.set_has_arrow(false);
            }
        }
        // Keep the hook installed.
        gtk4::glib::ffi::GTRUE
    }

    // SAFETY: "show" is a GtkWidget signal; the hook lives for the process.
    unsafe {
        let signal = gobject_ffi::g_signal_lookup(
            c"show".as_ptr(),
            <gtk4::Widget as gtk4::glib::prelude::StaticType>::static_type().into_glib(),
        );
        gobject_ffi::g_signal_add_emission_hook(signal, 0, Some(hook), std::ptr::null_mut(), None);
    }
}

fn raise_font_size() {
    let Some(settings) = gtk4::Settings::default() else {
        return;
    };
    let Some(name) = settings.gtk_font_name() else {
        return;
    };
    let mut font = gtk4::pango::FontDescription::from_string(&name);
    if font.size() < MIN_FONT_POINTS * gtk4::pango::SCALE {
        font.set_size(MIN_FONT_POINTS * gtk4::pango::SCALE);
        settings.set_gtk_font_name(Some(&font.to_string()));
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    // SAFETY: forwarding to the next window procedure, and reading the RECT
    // Windows passes with WM_MOVING.
    unsafe {
        match message {
            // A maximized window stays HTCLIENT: GTK's own drag restores it.
            WM_NCHITTEST => {
                let hit = DefSubclassProc(hwnd, message, wparam, lparam);
                if hit != HTCLIENT as LRESULT {
                    return hit;
                }
                let point = POINT {
                    x: (lparam & 0xFFFF) as i16 as i32,
                    y: ((lparam >> 16) & 0xFFFF) as i16 as i32,
                };
                if let Some(edge) = edge_hit(hwnd, point) {
                    return edge as LRESULT;
                }
                if IsZoomed(hwnd) == 0 && is_caption(hwnd, point) {
                    return HTCAPTION as LRESULT;
                }
                hit
            }
            WM_WINDOWPOSCHANGED => {
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                clip_to_content(hwnd);
                result
            }
            WM_NCLBUTTONDBLCLK if wparam == HTCAPTION as WPARAM => {
                with_window(|window| {
                    if window.is_maximized() {
                        window.unmaximize();
                    } else {
                        window.maximize();
                    }
                });
                0
            }
            WM_ENTERSIZEMOVE => {
                DRAG.with(|drag| {
                    drag.moved.set(false);
                    drag.shift.set(None);
                });
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_MOVING => {
                restore_while_moving(hwnd, &mut *(lparam as *mut RECT));
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_EXITSIZEMOVE => {
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                if DRAG.with(|drag| drag.moved.get()) {
                    snap(hwnd);
                }
                result
            }
            WM_ACTIVATE => {
                set_keyboard_hook(wparam & 0xFFFF != WA_INACTIVE as WPARAM);
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_DESTROY => {
                set_keyboard_hook(false);
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
            WM_WIN_ARROW => {
                win_arrow(hwnd, wparam as u16);
                0
            }
            // Alt+Space, at the visible top-left corner like Windows.
            WM_SYSCOMMAND if wparam & 0xFFF0 == SC_KEYMENU as WPARAM && lparam == 0x20 => {
                let mut frame = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(hwnd, &mut frame);
                let margin = margin(hwnd);
                let (x, y) = (frame.left + margin, frame.top + margin);
                let point = ((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF);
                SendMessageW(hwnd, WM_SYSMENU, 0, point as LPARAM)
            }
            WM_NCRBUTTONUP if wparam == HTCAPTION as WPARAM => {
                SendMessageW(hwnd, WM_SYSMENU, 0, lparam)
            }
            _ => DefSubclassProc(hwnd, message, wparam, lparam),
        }
    }
}

/// On the first move of a half-screen window, shrink it back to its restore
/// size around the cursor; then keep shifting the drag rectangle to match.
fn restore_while_moving(hwnd: HWND, rect: &mut RECT) {
    DRAG.with(|drag| {
        drag.moved.set(true);
        let Some((width, height)) = drag.restore_size.get() else {
            return;
        };
        let shift = match drag.shift.get() {
            Some(shift) => shift,
            None => {
                let mut cursor = POINT { x: 0, y: 0 };
                // SAFETY: an out-parameter, and resizing our own window.
                unsafe {
                    GetCursorPos(&mut cursor);
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        0,
                        0,
                        width,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                let fraction =
                    f64::from(cursor.x - rect.left) / f64::from((rect.right - rect.left).max(1));
                let left = cursor.x - (fraction * f64::from(width)) as i32;
                let shift = left - rect.left;
                drag.shift.set(Some(shift));
                drag.restore_size.set(None);
                drag.tiled.set(None);
                shift
            }
        };
        let top = rect.top;
        rect.left += shift;
        rect.right = rect.left + width;
        rect.bottom = top + height;
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

fn set_keyboard_hook(on: bool) {
    DRAG.with(|drag| {
        let hook = drag.hook.get();
        // SAFETY: installing or removing our own hook on this GUI thread,
        // whose message loop then runs it.
        unsafe {
            if on && hook.is_null() {
                drag.hook.set(SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(keyboard_hook),
                    std::ptr::null_mut(),
                    0,
                ));
            } else if !on && !hook.is_null() {
                UnhookWindowsHookEx(hook);
                drag.hook.set(std::ptr::null_mut());
            }
        }
    });
}

/// Takes Win+arrow (without other modifiers) and posts it to the window.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: Windows passes a KBDLLHOOKSTRUCT with keyboard hook calls.
    unsafe {
        if code >= 0 && (wparam == WM_KEYDOWN as WPARAM || wparam == WM_SYSKEYDOWN as WPARAM) {
            let key = &*(lparam as *const KBDLLHOOKSTRUCT);
            let held = |vk: u16| GetAsyncKeyState(i32::from(vk)) < 0;
            let arrow = key.vkCode as u16;
            if [VK_UP, VK_DOWN, VK_LEFT, VK_RIGHT].contains(&arrow)
                && (held(VK_LWIN) || held(VK_RWIN))
                && ![VK_SHIFT, VK_CONTROL, VK_MENU].into_iter().any(held)
            {
                keybd_event(VK_MASK, 0, 0, 0);
                keybd_event(VK_MASK, 0, KEYEVENTF_KEYUP, 0);
                PostMessageW(
                    DRAG.with(|drag| drag.hwnd.get()),
                    WM_WIN_ARROW,
                    arrow as WPARAM,
                    0,
                );
                return 1;
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

/// What Windows does for Win+arrow on a framed window, on one monitor.
fn win_arrow(hwnd: HWND, arrow: u16) {
    let tiled = DRAG.with(|drag| drag.tiled.get());
    with_window(|window| {
        let side = match arrow {
            VK_UP => {
                window.maximize();
                return;
            }
            VK_DOWN => {
                if window.is_maximized() {
                    window.unmaximize();
                } else if tiled.is_some() {
                    untile(hwnd);
                } else {
                    window.minimize();
                }
                return;
            }
            VK_LEFT => Side::Left,
            _ => Side::Right,
        };
        if window.is_maximized() {
            // Tile once GTK has laid out the restored window.
            window.unmaximize();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                tile_on_monitor(hwnd, side)
            });
        } else if tiled.is_some_and(|tiled| tiled != side) {
            untile(hwnd);
        } else if tiled.is_none() {
            tile_on_monitor(hwnd, side);
        }
    });
}

fn tile_on_monitor(hwnd: HWND, side: Side) {
    if let Some((_, work)) = frame_and_work_area(hwnd) {
        tile(hwnd, work, side);
    }
}

/// GTK opens windows at the top-left corner at their default size, even
/// where that runs off a small screen: center and fit the first one.
fn place_on_screen(hwnd: HWND) {
    let Some((frame, work)) = frame_and_work_area(hwnd) else {
        return;
    };
    let margin = margin(hwnd);
    let width = (frame.right - frame.left).min(work.right - work.left + 2 * margin);
    let height = (frame.bottom - frame.top).min(work.bottom - work.top + 2 * margin);
    // SAFETY: moving and resizing our own window.
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            (work.left + work.right - width) / 2,
            (work.top + work.bottom - height) / 2,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// The window's rectangle and the work area of the monitor it is mostly on.
fn frame_and_work_area(hwnd: HWND) -> Option<(RECT, RECT)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: out-parameters of the stated size.
    unsafe {
        GetWindowRect(hwnd, &mut frame);
        let center = POINT {
            x: (frame.left + frame.right) / 2,
            y: (frame.top + frame.bottom) / 2,
        };
        if GetMonitorInfoW(
            MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST),
            &mut info,
        ) == 0
        {
            return None;
        }
    }
    Some((frame, info.rcWork))
}

/// Back to the size and place the window had before it was tiled.
fn untile(hwnd: HWND) {
    let (size, origin) = DRAG.with(|drag| {
        drag.tiled.set(None);
        (drag.restore_size.take(), drag.restore_origin.take())
    });
    if let (Some((width, height)), Some((x, y))) = (size, origin) {
        // SAFETY: moving and resizing our own window.
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// After a move: cursor at the top edge maximizes, at a side edge tiles.
fn snap(hwnd: HWND) {
    let mut cursor = POINT { x: 0, y: 0 };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: out-parameters of the stated size.
    unsafe {
        GetCursorPos(&mut cursor);
        if GetMonitorInfoW(
            MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST),
            &mut info,
        ) == 0
        {
            return;
        }
    }
    let work = info.rcWork;
    if cursor.y <= work.top {
        // Not from inside the move loop's last message.
        gtk4::glib::idle_add_local_once(|| with_window(|window| window.maximize()));
    } else if cursor.x <= work.left {
        tile(hwnd, work, Side::Left);
    } else if cursor.x >= work.right - 1 {
        tile(hwnd, work, Side::Right);
    }
}

/// Fill half the work area with the window's visible part. GTK draws an
/// invisible shadow margin inside the window, which goes past the edges.
fn tile(hwnd: HWND, work: RECT, side: Side) {
    with_window(|window| {
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: an out-parameter.
        unsafe { GetWindowRect(hwnd, &mut frame) };
        let (frame_width, frame_height) = (frame.right - frame.left, frame.bottom - frame.top);
        let scale = f64::from(window.scale_factor());
        let (dx, dy) = window.surface_transform();
        let left = (dx * scale) as i32;
        let top = (dy * scale) as i32;
        let right = frame_width - left - (f64::from(window.width()) * scale) as i32;
        let bottom = frame_height - top - (f64::from(window.height()) * scale) as i32;

        DRAG.with(|drag| {
            if drag.restore_size.get().is_none() {
                drag.restore_size.set(Some((frame_width, frame_height)));
                drag.restore_origin.set(Some((frame.left, frame.top)));
            }
            drag.tiled.set(Some(side));
        });
        let half = (work.right - work.left) / 2;
        let x = match side {
            Side::Left => work.left,
            Side::Right => work.left + half,
        };
        // SAFETY: moving and resizing our own window.
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x - left,
                work.top - top,
                half + left + right,
                work.bottom - work.top + top + bottom,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    });
}

fn with_window(f: impl FnOnce(&gtk4::Window)) {
    if let Some(window) = DRAG.with(|drag| drag.window.borrow().as_ref().and_then(|w| w.upgrade()))
    {
        f(&window);
    }
}

/// Whether a screen point is on a header bar's empty space.
fn is_caption(hwnd: HWND, point: POINT) -> bool {
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: an out-parameter.
    unsafe { GetWindowRect(hwnd, &mut frame) };
    let mut caption = false;
    with_window(|window| {
        // The client area is the whole window: GTK draws its own frame.
        let scale = f64::from(window.scale_factor());
        let (dx, dy) = window.surface_transform();
        let Some(mut widget) = window.pick(
            f64::from(point.x - frame.left) / scale - dx,
            f64::from(point.y - frame.top) / scale - dy,
            gtk4::PickFlags::DEFAULT,
        ) else {
            return;
        };
        loop {
            if widget.is::<gtk4::WindowHandle>() {
                caption = true;
                return;
            }
            if is_interactive(&widget) {
                return;
            }
            match widget.parent() {
                Some(parent) => widget = parent,
                None => return,
            }
        }
    });
    caption
}

fn is_interactive(widget: &gtk4::Widget) -> bool {
    widget.is::<gtk4::Button>()
        || widget.is::<gtk4::MenuButton>()
        || widget.is::<gtk4::Editable>()
        || widget.is::<gtk4::DropDown>()
        || widget.is::<gtk4::Switch>()
        || widget.is::<gtk4::Scale>()
}
