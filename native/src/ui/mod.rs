// SPDX-License-Identifier: GPL-3.0-or-later

//! Native GTK4/libadwaita shell.
//!
//! The GTK thread owns widgets only. It receives immutable `AppSnapshot`s
//! through the capacity-one coordinator channel and dispatches typed
//! `Command`s back; every send is nonblocking. One drain consumes the
//! snapshot, tray-event, and coordinator-stopped receivers; it runs only when
//! a producer calls `wake_shell`, so an idle shell never polls. No stores, no
//! per-widget view models, no string events, no blocking work.

pub mod damage_meter;
pub mod filters;
pub mod library;
pub mod multipov;
pub mod operational_actions;
pub mod player;
pub mod player_backend;
pub mod settings;
pub mod sidebar;
pub mod status;
pub mod timeline;
#[cfg(unix)]
pub mod tray_backend;
#[cfg(windows)]
#[path = "tray_windows.rs"]
pub mod tray_backend;
pub mod window;
#[cfg(windows)]
mod windows_diag;
#[cfg(windows)]
pub use windows_diag::forward_glib_warnings;
#[cfg(windows)]
mod windows_native;
#[cfg(windows)]
pub mod windows_update;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[cfg(unix)]
use gtk4::glib::ffi;
use gtk4::prelude::*;
use libadwaita as adw;

use warcraft_rustcorder::config::LayoutSettings;
use warcraft_rustcorder::coordinator::{Command, CoordinatorHandle};
use warcraft_rustcorder::domain::{Category, Theme};

use tray_backend::{TrayBackend, TrayEvent};

/// Everything `run` needs that is not the coordinator or the tray.
pub struct ShellOptions {
    /// Recorder diagnostics directory (`gsr.log`, `app.log`, token).
    pub data_dir: PathBuf,
    /// Fallback directory for "Open logs" when `data_dir` does not exist yet.
    pub config_dir: PathBuf,
    /// Initial interface flags read before the GTK loop starts; live values
    /// arrive with the first snapshot.
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub minimize_to_tray: bool,
    pub theme: Theme,
}

/// The app's own theme setting overrides the system light/dark preference.
pub fn apply_theme(theme: Theme) {
    adw::StyleManager::default().set_color_scheme(match theme {
        Theme::Dark => adw::ColorScheme::ForceDark,
        Theme::Light => adw::ColorScheme::ForceLight,
    });
}

/// The one sink every widget uses to reach the coordinator or shell actions.
/// Returns `false` when the command channel was full (caller shows one Busy
/// problem and may disable the initiating widget briefly).
pub type ActionSink = Rc<dyn Fn(ShellAction) -> bool>;

thread_local! {
    /// The shell's receiver drain, reachable from the coordinator's wake
    /// callback. GLib only accepts `Send` closures from another thread, and
    /// the drain owns `Rc`s, so the closure looks the pump up here after GLib
    /// has already hopped it onto the main thread.
    static PUMP: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    /// Set by `request_quit`; the pump treats it like the tray's Quit.
    static QUIT_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Run the shell's drain on the main thread. Safe to call from any thread and
/// harmless before the pump is installed or after the loop has finished.
/// `pending` collapses a burst of wakes into one queued idle. Producers queue
/// their data before waking, and the latch is cleared before the drain reads
/// anything, so a wake that finds the latch set is covered by that drain.
/// The clearing swap acquires, which makes that data visible to the drain.
pub fn wake_shell(pending: &Arc<AtomicBool>) {
    if pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let pending = Arc::clone(pending);
    gtk4::glib::idle_add_once(move || {
        pending.swap(false, Ordering::AcqRel);
        run_pump();
    });
}

/// Quit the way the tray's Quit does: the coordinator ends and saves any
/// recording before the GTK loop exits. Call on the main thread.
pub fn request_quit() {
    QUIT_REQUESTED.set(true);
    run_pump();
}

fn run_pump() {
    let pump = PUMP.with(|pump| pump.borrow().clone());
    if let Some(pump) = pump {
        pump();
    }
}

// glib-rs 0.22 moved `unix_signal_add` to the separate glib-unix crate; the C
// function lives in the libglib that glib-sys already links.
#[cfg(unix)]
unsafe extern "C" {
    fn g_unix_signal_add(
        signum: i32,
        handler: extern "C" fn(ffi::gpointer) -> ffi::gboolean,
        data: ffi::gpointer,
    ) -> u32;
}

/// Runs on the main loop, not in signal context, so it may touch the pump.
#[cfg(unix)]
extern "C" fn on_sigterm(_: ffi::gpointer) -> ffi::gboolean {
    request_quit();
    ffi::G_SOURCE_CONTINUE
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShellAction {
    Command(Command),
    /// Maps `RecoveryAction::Retry`; the shell retries arming capture.
    Retry,
    OpenSettings,
    /// Opens the test-recording category chooser.
    TestRecording,
    OpenLogs,
    About,
    /// Runs the published install script to update to the latest Flatpak.
    CheckForUpdates,
    Quit,
}

/// A resize drag notifies per pixel; coalesce so one drag costs one write.
const LAYOUT_SAVE_DELAY: Duration = Duration::from_millis(750);

/// The divider and column widths the user dragged, adopted from the first
/// snapshot and written back through the coordinator on a debounce.
pub struct LayoutStore {
    sink: ActionSink,
    layout: RefCell<LayoutSettings>,
    save_queued: Cell<bool>,
}

impl LayoutStore {
    pub fn new(sink: ActionSink) -> Rc<Self> {
        Rc::new(Self {
            sink,
            layout: RefCell::new(LayoutSettings::default()),
            save_queued: Cell::new(false),
        })
    }

    pub fn adopt(&self, layout: &LayoutSettings) {
        self.layout.borrow_mut().clone_from(layout);
    }

    /// `None` means the user has never dragged the divider.
    pub fn player_split(&self) -> Option<i32> {
        self.layout.borrow().player_split
    }

    pub fn column_width(&self, title: &str) -> Option<i32> {
        self.layout.borrow().column_widths.get(title).copied()
    }

    pub fn set_player_split(self: &Rc<Self>, position: i32) {
        if self.layout.borrow().player_split == Some(position) {
            return;
        }
        self.layout.borrow_mut().player_split = Some(position);
        self.queue_save();
    }

    /// A negative width is GTK's double-click reset; drop the override.
    pub fn set_column_width(self: &Rc<Self>, title: &str, width: i32) {
        {
            let mut layout = self.layout.borrow_mut();
            let changed = if width < 0 {
                layout.column_widths.remove(title).is_some()
            } else {
                layout.column_widths.insert(title.to_owned(), width) != Some(width)
            };
            if !changed {
                return;
            }
        }
        self.queue_save();
    }

    fn queue_save(self: &Rc<Self>) {
        if self.save_queued.replace(true) {
            return;
        }
        let this = Rc::clone(self);
        gtk4::glib::timeout_add_local_once(LAYOUT_SAVE_DELAY, move || {
            this.save_queued.set(false);
            let layout = this.layout.borrow().clone();
            if !(this.sink)(ShellAction::Command(Command::SaveLayout { layout })) {
                // The command channel was full; retry rather than lose the drag.
                this.queue_save();
            }
        });
    }
}

/// Outcome of claiming the single application instance.
pub enum Registration {
    /// This process owns the application and must run the shell.
    Primary(adw::Application),
    /// Another process owns it; the caller must hand activation to the
    /// primary via `run_remote` and exit before touching shared state.
    Secondary(adw::Application),
}

/// Build the application, wire the startup handler (GResource, CSS, icons),
/// and register with the session bus. Registration must happen before any
/// work with side effects: a second launcher is reported as `Secondary` so it
/// can activate the primary and exit instead of rotating the shared log,
/// sweeping live capture files, or arming a second recorder. A successful
/// primary registration can emit `startup` immediately, which is why
/// resources and the provider hookup are connected before `register`.
pub fn register(app_id: &'static str) -> Result<Registration, gtk4::glib::Error> {
    register_ui_resources();

    let application = adw::Application::builder().application_id(app_id).build();

    application.connect_startup(|_| {
        let Some(display) = gtk4::gdk::Display::default() else {
            return;
        };
        let provider = gtk4::CssProvider::new();
        provider.load_from_resource("/io/github/JohanWes/WarcraftRustcorder/style.css");
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        gtk4::IconTheme::for_display(&display)
            .add_resource_path("/io/github/JohanWes/WarcraftRustcorder/icons");
    });

    application.register(None::<&gtk4::gio::Cancellable>)?;
    if application.is_remote() {
        Ok(Registration::Secondary(application))
    } else {
        Ok(Registration::Primary(application))
    }
}

/// `include_bytes!` is only byte-aligned; GIO copies unaligned resource data
/// to the heap, so over-align it to keep the embedded bundle zero-copy.
#[repr(C, align(8))]
struct Aligned<T: ?Sized>(T);

static UI_RESOURCES: &Aligned<[u8]> = &Aligned(*include_bytes!(concat!(
    env!("OUT_DIR"),
    "/warcraft-rustcorder.gresource"
)));

/// Register the embedded UI bundle (CSS, icons) that `startup` needs.
fn register_ui_resources() {
    let bytes = gtk4::glib::Bytes::from_static(&UI_RESOURCES.0);
    let resource =
        gtk4::gio::Resource::from_data(&bytes).expect("register compiled GResource bundle");
    gtk4::gio::resources_register(&resource);
}

/// Register the spell bundle by mmap. The Flatpak installs it under `/app`,
/// the Windows build in `share/` beside `bin/`; development runs fall back to
/// the copy `build.rs` compiled. Without
/// it the damage meter shows no spell icons or tooltips. Called from `run`,
/// after logging is up and only in the primary instance.
fn register_spell_resources() {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("share")))
        .unwrap_or_else(|| "/app/share".into())
        .join("warcraft-rustcorder/spells.gresource");
    let spells = [
        installed,
        concat!(env!("OUT_DIR"), "/spells.gresource").into(),
    ]
    .into_iter()
    .find_map(|path| gtk4::gio::Resource::load(path).ok());
    match spells {
        Some(resource) => gtk4::gio::resources_register(&resource),
        None => tracing::warn!("spell resource bundle not found; spell icons disabled"),
    }
}

/// Hand activation to the primary instance on the session bus. The remote
/// `run` skips the main loop and unregisters cleanly, so this returns as
/// soon as the primary has been notified; the return value is the exit code.
pub fn run_remote(application: adw::Application) -> i32 {
    application.run_with_args(&[env!("CARGO_PKG_NAME")]).into()
}

/// Category rail metadata in rail order: label and symbolic icon.
pub const CATEGORIES: [(Category, &str, &str); 10] = [
    (Category::TwoVTwo, "2v2", "wr-category-2v2-symbolic"),
    (Category::ThreeVThree, "3v3", "wr-category-3v3-symbolic"),
    (Category::FiveVFive, "5v5", "wr-category-5v5-symbolic"),
    (
        Category::Skirmish,
        "Skirmish",
        "wr-category-skirmish-symbolic",
    ),
    (
        Category::SoloShuffle,
        "Solo Shuffle",
        "wr-category-solo-shuffle-symbolic",
    ),
    (
        Category::MythicPlus,
        "Mythic+",
        "wr-category-mythic-plus-symbolic",
    ),
    (Category::Raids, "Raids", "wr-category-raids-symbolic"),
    (
        Category::Battlegrounds,
        "Battlegrounds",
        "wr-category-battlegrounds-symbolic",
    ),
    (Category::Manual, "Manual", "wr-category-manual-symbolic"),
    (Category::Clip, "Clips", "wr-category-clips-symbolic"),
];

/// Test-recording choices, in menu order.
pub const TEST_CATEGORIES: [Category; 6] = [
    Category::TwoVTwo,
    Category::ThreeVThree,
    Category::SoloShuffle,
    Category::Raids,
    Category::Battlegrounds,
    Category::MythicPlus,
];

pub fn category_label(category: &Category) -> &str {
    CATEGORIES
        .iter()
        .find(|(candidate, _, _)| candidate == category)
        .map(|(_, label, _)| *label)
        .unwrap_or("Recordings")
}

/// A duration as `m:ss`, or `h:mm:ss` from one hour.
pub fn format_clock(ms: u64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A flat icon-only button whose tooltip doubles as its accessible label.
pub fn icon_button(icon: &str, label: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk4::accessible::Property::Label(label)]);
    button
}

pub fn clear_box(container: &gtk4::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// Run the shell for the already-registered primary application; returns the
/// process exit code. The caller joins the coordinator and tray handles after
/// this returns.
pub fn run(
    application: adw::Application,
    options: &ShellOptions,
    coordinator: Rc<RefCell<CoordinatorHandle>>,
    tray: Option<Rc<TrayBackend>>,
    tray_events: Receiver<TrayEvent>,
) -> i32 {
    register_spell_resources();
    // SIGTERM (logout, `systemctl stop`, `kill`) takes the tray Quit path so
    // the coordinator stops gpu-screen-recorder instead of orphaning it.
    // SAFETY: a plain fn pointer and no data; GLib owns the source.
    #[cfg(unix)]
    unsafe {
        g_unix_signal_add(libc::SIGTERM, on_sigterm, std::ptr::null_mut())
    };
    let shell: Rc<RefCell<Option<window::Shell>>> = Rc::new(RefCell::new(None));
    {
        let shell_cell = Rc::clone(&shell);
        let activate_coordinator = Rc::clone(&coordinator);
        let activate_tray = tray.clone();
        let data_dir = options.data_dir.clone();
        let config_dir = options.config_dir.clone();
        let start_minimized = options.start_minimized;
        let close_to_tray = options.close_to_tray;
        let minimize_to_tray = options.minimize_to_tray;
        let theme = options.theme;
        application.connect_activate(move |application| {
            // Start-minimized is a first-activation flag only: a later
            // activation (second launcher invocation, tray "Open") must
            // reveal a window that is already running hidden.
            let first_activation = shell_cell.borrow().is_none();
            if first_activation {
                apply_theme(theme);
                let built = window::Shell::build(
                    application,
                    Rc::clone(&activate_coordinator),
                    activate_tray.clone(),
                    &data_dir,
                    &config_dir,
                    close_to_tray,
                    minimize_to_tray,
                );
                *shell_cell.borrow_mut() = Some(built);
            }
            let shell_ref = shell_cell.borrow();
            let built = shell_ref.as_ref().expect("shell built above");
            #[cfg(windows)]
            if first_activation {
                windows_update::check_quietly(built.widget());
            }
            // Only start hidden when a tray watcher can bring the window back.
            let watcher_available = activate_tray
                .as_ref()
                .is_some_and(|tray| tray.is_available());
            if first_activation && watcher_available && start_minimized {
                built.hide_to_tray();
            } else {
                built.present();
            }
            drop(shell_ref);
            // The drain leaves a snapshot queued while there is no shell yet,
            // and the coordinator will not wake it again for that snapshot.
            if first_activation {
                run_pump();
            }
        });
    }

    // The single drain: tray events, the newest snapshot, and the
    // coordinator-stopped signal. It runs once after the shell is built and
    // otherwise only on `wake_shell`: the coordinator calls it for every
    // snapshot and the stopped signal, the tray for Open, Quit, and watcher
    // availability changes.
    {
        let app = application.clone();
        let shutdown_sent = Cell::new(false);
        let coordinator_stopped = Cell::new(false);
        // Last tooltip pushed to the tray, so we only pay the cross-thread
        // `update` round-trip on a real change instead of every snapshot.
        let last_tray_title = RefCell::new(String::new());
        // Last availability fanned out to the widgets; `None` guarantees the
        // first pass propagates, including "no tray at all".
        let last_tray_available = Cell::new(None::<bool>);
        let pump: Rc<dyn Fn()> = Rc::new(move || {
            while let Ok(TrayEvent::Open) = tray_events.try_recv() {
                if let Some(shell) = shell.borrow().as_ref() {
                    shell.present();
                }
            }
            let quit_requested =
                QUIT_REQUESTED.get() || tray.as_ref().is_some_and(|tray| tray.quit_requested());
            if quit_requested && !shutdown_sent.get() {
                shutdown_sent.set(coordinator.borrow().send(Command::Shutdown));
            }
            // Borrow the shell first: a snapshot taken before the window
            // exists would be dropped, and the coordinator only republishes
            // when its state changes again.
            if let Some(shell) = shell.borrow().as_ref()
                && let Ok(snapshot) = coordinator.borrow().snapshots.try_recv()
            {
                shell.apply_snapshot(&snapshot);
                if let Some(tray) = &tray {
                    let title = format!("Warcraft Rustcorder: {}", status::view(&snapshot).title);
                    if *last_tray_title.borrow() != title {
                        tray.update(title.clone());
                        *last_tray_title.borrow_mut() = title;
                    }
                }
            }
            // A missing tray service is itself an availability the widgets
            // need, so this reads `false` rather than skipping the fan-out.
            // The cache is only committed once the widgets actually have it:
            // recording a transition the shell was too early to receive would
            // suppress it until availability happened to change again.
            let available = tray.as_ref().is_some_and(|tray| tray.is_available());
            if last_tray_available.get() != Some(available)
                && let Some(shell) = shell.borrow().as_ref()
            {
                shell.set_tray_available(available);
                last_tray_available.set(Some(available));
            }
            if !coordinator_stopped.get() && coordinator.borrow().stopped.try_recv().is_ok() {
                coordinator_stopped.set(true);
                app.quit();
            }
        });
        PUMP.with(|slot| *slot.borrow_mut() = Some(pump));
    }

    application.run_with_args(&[env!("CARGO_PKG_NAME")]).into()
}

fn simple_action(application: &adw::Application, name: &str, activate: impl Fn() + 'static) {
    let action = gtk4::gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| activate());
    application.add_action(&action);
}

/// Register the primary-menu actions shared by the shell.
pub fn install_actions(application: &adw::Application, sink: ActionSink) {
    for (name, action) in [
        ("settings", ShellAction::OpenSettings),
        ("about", ShellAction::About),
        ("open-logs", ShellAction::OpenLogs),
        ("test-recording", ShellAction::TestRecording),
    ] {
        let sink = Rc::clone(&sink);
        simple_action(application, name, move || {
            sink(action.clone());
        });
    }
}

/// The window primary menu: Test recording, Open logs, About. There is no
/// update UI; the Flatpak remote and software center own updates.
pub fn primary_menu() -> gtk4::gio::Menu {
    let menu = gtk4::gio::Menu::new();
    menu.append(Some("Test recording…"), Some("app.test-recording"));
    menu.append(Some("Open logs"), Some("app.open-logs"));
    menu.append(Some("About Warcraft Rustcorder"), Some("app.about"));
    menu
}
