// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows diagnostics for app.log, for reports from machines we cannot test
//! on: how GTK renders, each monitor's refresh rate and scale, frame pacing of
//! every animation or playback burst, and the process's CPU and memory use.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use libadwaita as adw;
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// A gap this long between frames ends a burst.
const BURST_GAP_US: i64 = 250_000;
/// Bursts shorter than this (hover fades) are not worth a line.
const MIN_BURST_FRAMES: u32 = 20;
/// Frames slower than this (under 40 fps) count as slow.
const SLOW_FRAME_US: i64 = 25_000;
const USAGE_INTERVAL: Duration = Duration::from_secs(60);

pub fn attach(window: &adw::ApplicationWindow) {
    // Realize happens once per window surface, map on every show from the
    // tray: hook the frame clock on the first, describe the setup only once.
    window.connect_realize(|window| {
        if let Some(clock) = window.frame_clock() {
            watch_frames(&clock);
        }
    });
    let described = Cell::new(false);
    window.connect_map(move |window| {
        if !described.replace(true) {
            log_setup(window);
        }
    });
    log_usage_periodically();
}

/// GTK, GLib and Clapper warnings go to app.log: the Windows build has no
/// console for them.
pub fn forward_glib_warnings() {
    gtk4::glib::log_set_writer_func(|level, fields| {
        use gtk4::glib::LogLevel;
        let field = |key: &str| {
            fields
                .iter()
                .find(|field| field.key() == key)
                .and_then(|field| field.value_str())
                .unwrap_or_default()
        };
        let (domain, message) = (field("GLIB_DOMAIN"), field("MESSAGE"));
        match level {
            LogLevel::Error | LogLevel::Critical => tracing::error!(domain, "{message}"),
            LogLevel::Warning => tracing::warn!(domain, "{message}"),
            LogLevel::Message => tracing::info!(domain, "{message}"),
            LogLevel::Info | LogLevel::Debug => {}
        }
        gtk4::glib::LogWriterOutput::Handled
    });
}

fn log_setup(window: &adw::ApplicationWindow) {
    let renderer = window
        .renderer()
        .map(|renderer| renderer.type_().name().to_owned())
        .unwrap_or_default();
    let settings = gtk4::Settings::default();
    tracing::info!(
        gtk = %format!(
            "{}.{}.{}",
            gtk4::major_version(),
            gtk4::minor_version(),
            gtk4::micro_version()
        ),
        renderer,
        font = settings.as_ref().and_then(|s| s.gtk_font_name()).unwrap_or_default().as_str(),
        xft_dpi = settings.as_ref().map_or(0, |s| s.gtk_xft_dpi()) / 1024,
        window_scale = window.scale_factor(),
        shadow_margin = window.surface_transform().0,
        animations = settings.as_ref().is_some_and(|s| s.is_gtk_enable_animations()),
        "windows display setup"
    );
    let display = WidgetExt::display(window);
    let monitors = display.monitors();
    for monitor in monitors.iter::<gtk4::gdk::Monitor>().flatten() {
        let geometry = monitor.geometry();
        tracing::info!(
            model = monitor.model().unwrap_or_default().as_str(),
            width = geometry.width(),
            height = geometry.height(),
            refresh_hz = f64::from(monitor.refresh_rate()) / 1000.0,
            scale = monitor.scale_factor(),
            "monitor"
        );
    }
}

#[derive(Default)]
struct Burst {
    start: i64,
    last: i64,
    frames: u32,
    slow: u32,
    worst: i64,
}

/// Logs each run of consecutive frames: count, rate, slow frames, and the
/// worst gap. A choppy animation shows up as a low rate or many slow frames.
fn watch_frames(clock: &gtk4::gdk::FrameClock) {
    let burst = Rc::new(Cell::new(Burst::default()));
    clock.connect_after_paint(move |_| {
        let now = gtk4::glib::monotonic_time();
        let mut current = burst.take();
        let gap = now - current.last;
        if gap > BURST_GAP_US {
            if current.frames >= MIN_BURST_FRAMES {
                let seconds = (current.last - current.start) as f64 / 1e6;
                tracing::info!(
                    frames = current.frames,
                    seconds = format!("{seconds:.2}"),
                    fps = format!("{:.1}", f64::from(current.frames - 1) / seconds),
                    slow_frames = current.slow,
                    worst_ms = current.worst / 1000,
                    "ui frame burst"
                );
            }
            current = Burst {
                start: now,
                ..Burst::default()
            };
        } else {
            current.worst = current.worst.max(gap);
            current.slow += u32::from(gap > SLOW_FRAME_US);
        }
        current.frames += 1;
        current.last = now;
        burst.set(current);
    });
}

fn log_usage_periodically() {
    let cores = std::thread::available_parallelism().map_or(1, usize::from) as f64;
    let last = Rc::new(Cell::new((cpu_time_100ns(), gtk4::glib::monotonic_time())));
    gtk4::glib::timeout_add_local(USAGE_INTERVAL, move || {
        let (cpu, at) = (cpu_time_100ns(), gtk4::glib::monotonic_time());
        let (previous_cpu, previous_at) = last.replace((cpu, at));
        let busy_us = (cpu - previous_cpu) as f64 / 10.0;
        let percent = busy_us / ((at - previous_at) as f64 * cores) * 100.0;
        tracing::info!(
            cpu_percent = format!("{percent:.1}"),
            working_set_mb = working_set_bytes() / (1024 * 1024),
            "app usage"
        );
        gtk4::glib::ControlFlow::Continue
    });
}

/// Kernel plus user time of this process, in 100 ns units.
fn cpu_time_100ns() -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the current-process pseudo handle and four out-parameters.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        );
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    ticks(kernel) + ticks(user)
}

fn working_set_bytes() -> usize {
    // SAFETY: a zeroed out-parameter of the stated size.
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb);
        counters.WorkingSetSize
    }
}
