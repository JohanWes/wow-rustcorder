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
//! GTK also takes Windows' 9 pt message font, which reads small next to the
//! 11 pt the layout was designed with, so the font is raised to that.
//!
//! With DirectComposition on (see `enable_gpu_rendering` in main.rs), GTK's
//! GL renderer draws transparent pixels black. So the window is clipped to its
//! visible part with a window region, which cuts off GTK's invisible resize
//! margin: resizing grabs the visible edge instead. Shadows and rounded
//! corners, which would show as black, are styled away.

use std::cell::{Cell, RefCell};

use gtk4::glib::object::ObjectType;
use gtk4::prelude::*;
use libadwaita as adw;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateRectRgn, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    SetWindowRgn,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT,
    HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsIconic, IsZoomed, SW_RESTORE, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
    WM_MOVING, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_WINDOWPOSCHANGED,
};

const MIN_FONT_POINTS: i32 = 11;
/// GTK's resize margin around a window without a shadow (RESIZE_HANDLE_SIZE).
const MARGIN: i32 = 12;
/// How far into the visible edge a resize grab reaches.
const GRIP: i32 = 4;

const CSS: &str = "
window.csd { box-shadow: none; border-radius: 0; }
popover > contents, popover > arrow { box-shadow: none; }
tooltip { box-shadow: none; }
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
            SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
            clip_to_content(hwnd);
        }
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
                shift
            }
        };
        let top = rect.top;
        rect.left += shift;
        rect.right = rect.left + width;
        rect.bottom = top + height;
    });
}

enum Side {
    Left,
    Right,
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
            }
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
