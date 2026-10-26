// SPDX-License-Identifier: GPL-3.0-or-later

//! Process entry point. `main` starts the coordinator and the tray service,
//! runs the GTK shell, then joins both top-level handles after the
//! application loop returns. No child or thread is detached.

// No console window behind the GUI on Windows; logs go to app.log.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

use warcraft_recorder::config::Config;
use warcraft_recorder::coordinator;

#[cfg(windows)]
mod clapper_stubs;
mod ui;

use ui::tray_backend::TrayBackend;

#[cfg(not(feature = "development"))]
const APP_ID: &str = warcraft_recorder::config::APP_ID;
#[cfg(feature = "development")]
const APP_ID: &str = "io.github.JohanWes.WarcraftRecorder.Devel";

fn main() {
    #[cfg(windows)]
    {
        enable_gpu_rendering();
        isolate_gstreamer();
    }
    let setup = match coordinator::Setup::from_environment() {
        Ok(setup) => setup,
        Err(error) => {
            tracing_subscriber::fmt::init();
            tracing::error!(%error, "cannot resolve the configuration directory");
            eprintln!("warcraft-recorder: {error}");
            std::process::exit(1);
        }
    };

    // Claim the single instance before anything with side effects runs: a
    // second launcher must only activate the primary and exit, or it would
    // rotate the shared log, sweep live capture files into Recovery, and arm
    // a second gpu-screen-recorder against the shared IPC socket.
    let application = match ui::register(APP_ID) {
        Ok(ui::Registration::Primary(application)) => application,
        Ok(ui::Registration::Secondary(application)) => {
            std::process::exit(ui::run_remote(application));
        }
        Err(error) => {
            eprintln!("warcraft-recorder: cannot register the application: {error}");
            std::process::exit(1);
        }
    };

    init_logging(&setup.data_dir);
    log_panics();
    #[cfg(windows)]
    ui::forward_glib_warnings();
    // Loaded once: the shell reads its startup flags, the coordinator the rest.
    let config = Config::load(&setup.config_path);
    let options = shell_options(&setup, config.as_ref().ok());

    // One latch shared by everything that can make the shell's drain useful,
    // so a burst of wakes costs a single queued main-loop callback.
    let wake_pending = Arc::new(AtomicBool::new(false));
    let wake: Arc<dyn Fn() + Send + Sync> = {
        let pending = Arc::clone(&wake_pending);
        Arc::new(move || ui::wake_shell(&pending))
    };
    let coordinator = Rc::new(RefCell::new(coordinator::start(setup, config, {
        let wake = Arc::clone(&wake);
        Box::new(move || wake())
    })));

    let (tray_events_tx, tray_events_rx) = mpsc::sync_channel(8);
    let tray = TrayBackend::start(tray_events_tx, Arc::clone(&wake))
        .map(Rc::new)
        .map_err(|error| tracing::warn!(%error, "tray service unavailable"))
        .ok();

    tracing::info!(application_id = APP_ID, "starting application");
    let code = ui::run(
        application,
        &options,
        Rc::clone(&coordinator),
        tray.clone(),
        tray_events_rx,
    );

    // The GTK loop has exited. Take the tray icon down first: the coordinator's
    // shutdown now waits out gpu-screen-recorder's flush, and an icon that
    // outlives the window by seconds is one the user can click for nothing.
    if let Some(tray) = tray {
        tray.shutdown();
    }
    coordinator.borrow_mut().shutdown();
    std::process::exit(code);
}

/// The shell options that come from outside the GTK loop: paths the shell
/// launches but never reads, plus the initial interface flags read once
/// before the loop starts (live values arrive with the first snapshot).
fn shell_options(setup: &coordinator::Setup, config: Option<&Config>) -> ui::ShellOptions {
    let config = config.cloned().unwrap_or_default();
    ui::ShellOptions {
        data_dir: setup.data_dir.clone(),
        config_dir: config_dir(&setup.config_path),
        start_minimized: config.interface.start_minimized,
        close_to_tray: config.interface.close_to_tray,
        minimize_to_tray: config.interface.minimize_to_tray,
    }
}

fn config_dir(config_path: &std::path::Path) -> PathBuf {
    config_path
        .parent()
        .map_or_else(PathBuf::new, std::path::Path::to_owned)
}

/// GTK 4.24 renders with OpenGL or Vulkan on Windows only through
/// DirectComposition, which it leaves off by default; without it every frame
/// is drawn by the CPU. Runs before any other thread exists.
#[cfg(windows)]
fn enable_gpu_rendering() {
    let mut flags = std::env::var("GDK_DEBUG").unwrap_or_default();
    if !flags.split(',').any(|flag| flag == "dcomp") {
        if !flags.is_empty() {
            flags.push(',');
        }
        flags.push_str("dcomp");
    }
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe { std::env::set_var("GDK_DEBUG", flags) };
}

/// A panic is logged before the default hook prints it: the Windows build
/// has no console, so stderr goes nowhere.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "panic");
        default(info);
    }));
}

/// The installed app (`bin/` beside `lib/gstreamer-1.0/`) loads only its own
/// GStreamer plugins, with a registry of its own, so another GStreamer on the
/// machine can neither leak plugins in nor share a stale registry. Runs before
/// any other thread exists.
#[cfg(windows)]
fn isolate_gstreamer() {
    let Some(root) = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.to_path_buf()))
    else {
        return;
    };
    let plugins = root.join("lib").join("gstreamer-1.0");
    if !plugins.is_dir() {
        return;
    }
    let registry = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone())
        .join(warcraft_recorder::config::APP_ID)
        .join("gstreamer-registry.bin");
    let scanner = root.join("libexec/gstreamer-1.0/gst-plugin-scanner.exe");
    // Clapper's own Windows lookup resolves relative to lib/gstreamer-1.0.
    let importers = root.join("lib/clapper-0.0/gst/plugin/importers");
    // SAFETY: single-threaded here; nothing else reads the environment yet.
    unsafe {
        std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &plugins);
        std::env::remove_var("GST_PLUGIN_PATH_1_0");
        std::env::remove_var("GST_PLUGIN_PATH");
        std::env::set_var("GST_REGISTRY_1_0", registry);
        std::env::set_var("GST_PLUGIN_SCANNER_1_0", scanner);
        std::env::set_var("CLAPPER_SINK_IMPORTER_PATH", importers);
    }
}

/// App log size cap; the previous log is kept as `.old`, so on-disk logging
/// stays bounded to two files.
const LOG_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

/// `tracing` output goes to `app.log` next to the recorder diagnostics that
/// "Open logs" reveals; stderr is the fallback when the file cannot open.
fn init_logging(data_dir: &std::path::Path) {
    let path = data_dir.join("app.log");
    let open = |path: &std::path::Path| {
        std::fs::create_dir_all(data_dir)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    };
    match open(&path) {
        Ok(file) => {
            let writer = RotatingLog {
                path,
                file,
                written: 0,
            };
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(writer))
                .init();
        }
        Err(error) => {
            tracing_subscriber::fmt::init();
            tracing::warn!(%error, "file logging unavailable; logging to stderr");
        }
    }
}

struct RotatingLog {
    path: PathBuf,
    file: std::fs::File,
    /// Bytes in the current file; seeded lazily from metadata on first write.
    written: u64,
}

impl std::io::Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written == 0 {
            self.written = self.file.metadata().map(|meta| meta.len()).unwrap_or(0) + 1;
        }
        if self.written.saturating_add(buf.len() as u64) > LOG_LIMIT_BYTES {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.old"));
            self.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.written = 1;
        }
        let count = self.file.write(buf)?;
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
