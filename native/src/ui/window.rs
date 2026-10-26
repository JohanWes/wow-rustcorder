// SPDX-License-Identifier: GPL-3.0-or-later

//! The one `AdwApplicationWindow`: `AdwNavigationSplitView` with the compact
//! sidebar and a content pane holding banners, the player above a draggable
//! divider, and the library toolbar/table below.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use warcraft_recorder::coordinator::{AppSnapshot, Command, CoordinatorHandle};
use warcraft_recorder::domain::{Category, RecoveryAction};

use warcraft_recorder::domain::RecorderStatus;

use super::library::{Library, Selection};
use super::operational_actions::{
    ManualBar, present_release_notes, present_test_dialog, present_update_dialog, release_notes,
};
use super::player::Player;
use super::settings::Settings;
use super::sidebar::Sidebar;
use super::status::{recovery_label, shell_action};
use super::tray_backend::TrayBackend;
use super::{ActionSink, LayoutStore, ShellAction, category_label, install_actions, primary_menu};

/// Starting divider position when nothing is stored and nothing is playing.
const DEFAULT_PLAYER_SPLIT: i32 = 400;

/// The content pane as rendered from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentView {
    pub title: String,
    pub setup_banner: Option<String>,
    pub problem_banner: Option<ProblemBanner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemBanner {
    pub summary: String,
    pub action: Option<RecoveryAction>,
}

pub fn content_view(snapshot: &AppSnapshot) -> ContentView {
    let selected = &snapshot.config.interface.selected_category;
    ContentView {
        title: category_label(selected).to_owned(),
        setup_banner: snapshot
            .setup_problems
            .first()
            .map(|problem| problem.message.clone()),
        problem_banner: snapshot.problems.last().map(|problem| ProblemBanner {
            summary: problem.summary.clone(),
            action: problem.recovery_action,
        }),
    }
}

/// Viewport height that shows `video_width`x`video_height` without side bars.
fn fitted_video_height(viewport_width: i32, video_width: u32, video_height: u32) -> i32 {
    if viewport_width <= 0 || video_width == 0 || video_height == 0 {
        return 0;
    }
    ((i64::from(viewport_width) * i64::from(video_height) + i64::from(video_width) - 1)
        / i64::from(video_width))
    .min(i64::from(i32::MAX)) as i32
}

/// The player/library divider. Sizing the pane to the video's aspect ratio is
/// a clean-start convenience only: the first drag hands ownership to the user
/// for good.
struct PaneLayout {
    paned: gtk4::Paned,
    store: Rc<LayoutStore>,
    /// Set while we move the divider ourselves.
    programmatic: Rc<Cell<bool>>,
    autoscale: Cell<bool>,
    /// Viewport width at the last fit; a change means the window resized, and
    /// that correction is animated rather than snapped.
    last_fit_width: Cell<i32>,
    /// `GtkPaned` rescales the divider whenever its own height changes and
    /// notifies exactly as a drag does, so only an unchanged height is a drag.
    last_height: Cell<i32>,
}

impl PaneLayout {
    fn set_position(&self, position: i32) {
        self.programmatic.set(true);
        self.paned.set_position(position);
        self.programmatic.set(false);
    }

    /// Adopt the persisted split, or leave autoscaling on for a clean start.
    fn restore(&self) {
        self.last_height.set(self.paned.height());
        match self.store.player_split() {
            Some(position) => {
                self.autoscale.set(false);
                self.set_position(position);
            }
            None => self.autoscale.set(true),
        }
    }

    /// Keep the baseline current for relayouts that moved no divider.
    fn sync_height(&self) {
        self.last_height.set(self.paned.height());
    }

    fn watch_position(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.paned.connect_position_notify(move |paned| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let height = paned.height();
            let relayout = this.last_height.replace(height) != height;
            if this.programmatic.get() || relayout {
                return;
            }
            this.autoscale.set(false);
            this.store.set_player_split(paned.position());
        });
    }

    /// Nudge the pane by the viewport's own error instead of predicting a
    /// height, so the real allocation self-corrects chrome and fullscreen
    /// changes in one pass.
    fn fit(&self, player: &Player, dimensions: (u32, u32), animation: &adw::TimedAnimation) {
        if !self.autoscale.get() || animation.state() == adw::AnimationState::Playing {
            return;
        }
        let (width, height) = player.viewport_size();
        let desired = fitted_video_height(width, dimensions.0, dimensions.1);
        if desired <= 0 || desired == height {
            self.last_fit_width.set(width);
            return;
        }
        let position = (self.paned.position() + desired - height)
            .max(0)
            .min(self.paned.max_position());
        if self.last_fit_width.replace(width) == width {
            self.set_position(position);
            return;
        }
        animation.set_value_from(f64::from(self.paned.position()));
        animation.set_value_to(f64::from(position));
        animation.play();
    }
}

pub struct Shell {
    window: adw::ApplicationWindow,
    sidebar: Sidebar,
    title: adw::WindowTitle,
    nav_page: adw::NavigationPage,
    setup_banner: adw::Banner,
    problem_banner: adw::Banner,
    problem_action: Rc<Cell<Option<RecoveryAction>>>,
    library: Library,
    player: Rc<Player>,
    manual_bar: ManualBar,
    layout: Rc<LayoutStore>,
    pane: Rc<PaneLayout>,
    layout_adopted: Cell<bool>,
    toasts: adw::ToastOverlay,
    /// Clips in the library at the last snapshot, to notice a finished clip.
    clip_count: Cell<Option<usize>>,
    settings: Rc<RefCell<Option<Rc<Settings>>>>,
    latest_snapshot: Rc<RefCell<Option<Arc<AppSnapshot>>>>,
    close_to_tray: Rc<Cell<bool>>,
    minimize_to_tray: Rc<Cell<bool>>,
    tray_available: Rc<Cell<bool>>,
    hold_guard: Rc<RefCell<Option<gtk4::gio::ApplicationHoldGuard>>>,
    sink: ActionSink,
    /// One release-notes dialog per process, on the same terms.
    release_notes_shown: Cell<bool>,
}

impl Shell {
    pub fn build(
        application: &adw::Application,
        coordinator: Rc<RefCell<CoordinatorHandle>>,
        tray: Option<Rc<TrayBackend>>,
        data_dir: &Path,
        config_dir: &Path,
        close_to_tray: bool,
        minimize_to_tray: bool,
    ) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Warcraft Recorder")
            .default_width(1440)
            .default_height(900)
            .width_request(640)
            .height_request(480)
            .build();
        #[cfg(windows)]
        {
            super::windows_native::setup(&window);
            super::windows_diag::attach(&window);
        }

        // Mirror the system theme onto the window so project CSS can darken
        // the class/outcome colors for light-theme contrast (WCAG AA).
        let style_manager = adw::StyleManager::default();
        let sync_light = {
            let window = window.clone();
            move |style: &adw::StyleManager| {
                if style.is_dark() {
                    window.remove_css_class("wr-light");
                } else {
                    window.add_css_class("wr-light");
                }
            }
        };
        sync_light(&style_manager);
        style_manager.connect_dark_notify(sync_light);

        // Transient feedback floats over the content instead of pushing the
        // player and table down.
        let toasts = adw::ToastOverlay::new();
        let settings_cell: Rc<RefCell<Option<Rc<Settings>>>> = Rc::new(RefCell::new(None));
        let latest_snapshot: Rc<RefCell<Option<Arc<AppSnapshot>>>> = Rc::new(RefCell::new(None));
        let tray_available = Rc::new(Cell::new(
            tray.as_ref().is_some_and(|tray| tray.is_available()),
        ));
        let sink = make_sink(
            &window,
            application,
            &coordinator,
            &toasts,
            data_dir,
            config_dir,
            &settings_cell,
            &latest_snapshot,
            &tray_available,
        );
        install_actions(application, Rc::clone(&sink));

        let sidebar = Sidebar::new(Rc::clone(&sink));

        // Content header: category title and the one primary menu.
        let title = adw::WindowTitle::new("Warcraft Recorder", "");
        let menu_button = gtk4::MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.set_menu_model(Some(&primary_menu()));
        menu_button.set_tooltip_text(Some("Main menu"));
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        header.pack_end(&menu_button);

        // Banners: setup and newest problem, both persistent state.
        let setup_banner = adw::Banner::new("");
        setup_banner.set_button_label(Some("Open Settings"));
        {
            let sink = Rc::clone(&sink);
            setup_banner.connect_button_clicked(move |_| {
                sink(ShellAction::OpenSettings);
            });
        }
        let problem_banner = adw::Banner::new("");
        // `adw::Banner` only exposes its button label, so the typed action is
        // carried here rather than reverse-mapped from displayed English.
        let problem_action: Rc<Cell<Option<RecoveryAction>>> = Rc::default();
        {
            let sink = Rc::clone(&sink);
            let problem_action = Rc::clone(&problem_action);
            problem_banner.connect_button_clicked(move |_| {
                if let Some(action) = problem_action.get() {
                    sink(shell_action(action));
                }
            });
        }

        // The Clapper player pane above the library table.
        let player = Rc::new(Player::new(Rc::clone(&sink)));
        player.install_shortcuts(window.upcast_ref());
        player.widget.set_height_request(240);
        let on_select: Rc<dyn Fn(Option<Selection>)> = {
            let player = Rc::clone(&player);
            Rc::new(move |selection| player.set_selection(selection.as_ref()))
        };
        let layout = LayoutStore::new(Rc::clone(&sink));
        let library = Library::new(Rc::clone(&sink), Rc::clone(&layout), on_select);

        let paned = gtk4::Paned::new(gtk4::Orientation::Vertical);
        paned.set_wide_handle(true);
        paned.set_vexpand(true);
        paned.set_start_child(Some(&player.widget));
        paned.set_end_child(Some(&library.widget));
        paned.set_resize_start_child(true);
        paned.set_shrink_start_child(false);
        paned.set_resize_end_child(true);
        paned.set_shrink_end_child(false);

        // Match the pane to the active recording instead of leaving Clapper
        // centered inside a fixed-height viewport with black side bars, until
        // the user sizes the divider, after which `PaneLayout` stands down.
        let video_dimensions = Rc::new(Cell::new(None::<(u32, u32)>));
        let programmatic = Rc::new(Cell::new(false));
        let fit_animation = adw::TimedAnimation::new(
            &paned,
            0.0,
            0.0,
            200,
            adw::CallbackAnimationTarget::new({
                let weak_paned = paned.downgrade();
                let programmatic = Rc::clone(&programmatic);
                move |value| {
                    if let Some(paned) = weak_paned.upgrade() {
                        programmatic.set(true);
                        paned.set_position(value as i32);
                        programmatic.set(false);
                    }
                }
            }),
        );
        let pane = Rc::new(PaneLayout {
            paned: paned.clone(),
            store: Rc::clone(&layout),
            programmatic,
            autoscale: Cell::new(true),
            last_fit_width: Cell::new(0),
            last_height: Cell::new(0),
        });
        pane.watch_position();
        {
            // Only the starting split for an empty player with nothing stored;
            // once a video is playing the viewport probe drives the position
            // from real allocations, and realize fires mid-transition with
            // stale ones.
            let weak_pane = Rc::downgrade(&pane);
            let video_dimensions = Rc::clone(&video_dimensions);
            paned.connect_realize(move |_| {
                let Some(pane) = weak_pane.upgrade() else {
                    return;
                };
                if pane.store.player_split().is_none() && video_dimensions.get().is_none() {
                    pane.set_position(DEFAULT_PLAYER_SPLIT);
                }
            });
        }
        {
            let weak_pane = Rc::downgrade(&pane);
            let weak_player = Rc::downgrade(&player);
            let video_dimensions = Rc::clone(&video_dimensions);
            let fit_animation = fit_animation.clone();
            player.connect_video_dimensions(move |width, height| {
                let dimensions = (width, height);
                video_dimensions.set(Some(dimensions));
                if let (Some(pane), Some(player)) = (weak_pane.upgrade(), weak_player.upgrade()) {
                    pane.fit(&player, dimensions, &fit_animation);
                }
            });
        }
        {
            let weak_pane = Rc::downgrade(&pane);
            let weak_player = Rc::downgrade(&player);
            let video_dimensions = Rc::clone(&video_dimensions);
            let fit_animation = fit_animation.clone();
            player.connect_viewport_resize(move || {
                let Some(pane) = weak_pane.upgrade() else {
                    return;
                };
                pane.sync_height();
                if let (Some(dimensions), Some(player)) =
                    (video_dimensions.get(), weak_player.upgrade())
                {
                    // Fullscreen hides the library, so the pane position no
                    // longer drives the viewport; let the video letterbox.
                    if pane
                        .paned
                        .end_child()
                        .is_some_and(|child| child.is_visible())
                    {
                        pane.fit(&player, dimensions, &fit_animation);
                    }
                }
            });
        }

        let banner_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        banner_box.append(&setup_banner);
        banner_box.append(&problem_banner);
        // The Manual-category start/stop entry.
        let manual_bar = ManualBar::new(Rc::clone(&sink));
        // One container for everything above the paned, so fullscreen can hide
        // it without fighting the banners'/manual bar's own visibility logic.
        let chrome = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        chrome.append(&banner_box);
        chrome.append(&manual_bar.widget);
        let content_body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_body.append(&chrome);
        content_body.append(&paned);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toasts.set_child(Some(&content_body));
        toolbar_view.set_content(Some(&toasts));
        let nav_page = adw::NavigationPage::new(&toolbar_view, "Warcraft Recorder");

        let sidebar_page = adw::NavigationPage::new(&sidebar.widget, "Categories");
        let split = adw::NavigationSplitView::new();
        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&nav_page));
        window.set_content(Some(&split));

        // VLC-style fullscreen: while the window is fullscreened only the
        // player pane (video, timeline, controls) stays visible; the sidebar,
        // header, banners, and library return on exit.
        {
            let split = split.clone();
            let toolbar_view = toolbar_view.clone();
            let chrome = chrome.clone();
            let library_widget = library.widget.clone();
            let player = Rc::clone(&player);
            window.connect_fullscreened_notify(move |window| {
                let fullscreen = window.is_fullscreen();
                if fullscreen {
                    split.set_show_content(true);
                }
                split.set_collapsed(fullscreen);
                toolbar_view.set_reveal_top_bars(!fullscreen);
                chrome.set_visible(!fullscreen);
                library_widget.set_visible(!fullscreen);
                player.set_fullscreen(fullscreen);
            });
        }

        let shell = Self {
            window: window.clone(),
            sidebar,
            title,
            nav_page,
            setup_banner,
            problem_banner,
            problem_action,
            library,
            player,
            manual_bar,
            layout,
            pane,
            layout_adopted: Cell::new(false),
            toasts,
            clip_count: Cell::new(None),
            settings: settings_cell,
            latest_snapshot,
            close_to_tray: Rc::new(Cell::new(close_to_tray)),
            minimize_to_tray: Rc::new(Cell::new(minimize_to_tray)),
            tray_available,
            hold_guard: Rc::new(RefCell::new(None)),
            sink,
            release_notes_shown: Cell::new(false),
        };
        shell.connect_close_request();
        shell.connect_minimize();
        shell
    }

    #[cfg(windows)]
    pub fn widget(&self) -> &gtk4::Widget {
        self.window.upcast_ref()
    }

    pub fn present(&self) {
        self.hold_guard.borrow_mut().take();
        // A window hidden while minimized keeps MINIMIZED on its toplevel, so
        // `present` alone re-maps it still minimized. Clear it first.
        self.window.unminimize();
        self.window.present();
    }

    /// Hide to the tray and hold the application while hidden. The
    /// coordinator and screen capture keep running.
    pub fn hide_to_tray(&self) {
        hide_and_hold(&self.window, &self.hold_guard, &self.player);
    }

    pub fn set_tray_available(&self, available: bool) {
        self.tray_available.set(available);
        self.sidebar.status_card.set_tray_available(available);
        if let Some(settings) = self.settings.borrow().as_ref() {
            settings.set_tray_available(available);
        }
        // If the tray vanished while the window was hidden, the window is the
        // only way back in, so reveal it: the process cannot be stranded. Bind
        // the borrow to a local first so it is released before `present` takes
        // the guard mutably.
        let hidden = self.hold_guard.borrow().is_some();
        if !available && hidden {
            self.present();
        }
    }

    pub fn apply_snapshot(&self, snapshot: &Arc<AppSnapshot>) {
        // The stored layout arrives with the first snapshot, before the library
        // builds the columns that read it.
        if !self.layout_adopted.replace(true) {
            self.layout.adopt(&snapshot.config.interface.layout);
            self.pane.restore();
        }
        self.close_to_tray
            .set(snapshot.config.interface.close_to_tray);
        self.minimize_to_tray
            .set(snapshot.config.interface.minimize_to_tray);

        self.sidebar.apply(snapshot);
        self.sidebar.status_card.apply(snapshot);

        let view = content_view(snapshot);
        self.title.set_title(&view.title);
        self.nav_page.set_title(&view.title);
        // The player consumes the snapshot first so selection callbacks fired
        // by the library rebuild resolve against current entries.
        self.player.apply_snapshot(snapshot);
        self.library.apply(snapshot);
        self.manual_bar.apply(snapshot);
        self.notice_new_clips(snapshot);

        *self.latest_snapshot.borrow_mut() = Some(Arc::clone(snapshot));
        if let Some(settings) = self.settings.borrow().as_ref() {
            settings.apply_snapshot(snapshot);
        }

        self.setup_banner.set_revealed(view.setup_banner.is_some());
        if let Some(message) = &view.setup_banner {
            self.setup_banner.set_title(message);
        }
        match &view.problem_banner {
            Some(problem) => {
                self.problem_banner.set_title(&problem.summary);
                self.problem_banner
                    .set_button_label(problem.action.map(recovery_label));
                self.problem_action.set(problem.action);
                self.problem_banner.set_revealed(true);
            }
            None => {
                self.problem_action.set(None);
                self.problem_banner.set_revealed(false);
            }
        }

        // Last, so a notice lands over a window that already shows the real
        // state behind it.
        if snapshot.config.last_seen_version != warcraft_recorder::VERSION
            && !self.release_notes_shown.replace(true)
        {
            let notes = release_notes(warcraft_recorder::VERSION);
            if notes.is_empty() {
                // A build with no recorded commits: acknowledge it silently so
                // the check stops firing on every snapshot.
                (self.sink)(ShellAction::Command(Command::DismissReleaseNotes));
            } else {
                present_release_notes(self.window.upcast_ref(), Rc::clone(&self.sink), &notes);
            }
        }
    }

    /// Clips are only ever added by a finished clip job, so a larger count
    /// than the previous snapshot's means one was just saved.
    fn notice_new_clips(&self, snapshot: &AppSnapshot) {
        let count = snapshot
            .category_counts
            .iter()
            .find(|(category, _)| *category == Category::Clip)
            .map_or(0, |(_, count)| *count);
        let previous = self.clip_count.replace(Some(count));
        if previous.is_none_or(|previous| count <= previous) {
            return;
        }
        let toast = adw::Toast::new("Clip saved");
        if snapshot.config.interface.selected_category != Category::Clip {
            toast.set_button_label(Some("Show"));
            let sink = Rc::clone(&self.sink);
            toast.connect_button_clicked(move |_| {
                sink(ShellAction::Command(Command::SetSelectedCategory {
                    category: Category::Clip,
                }));
            });
        }
        self.toasts.add_toast(toast);
    }

    fn connect_close_request(&self) {
        let tray_available = Rc::clone(&self.tray_available);
        let close_to_tray = Rc::clone(&self.close_to_tray);
        let hold_guard = Rc::clone(&self.hold_guard);
        let player = Rc::clone(&self.player);
        self.window.connect_close_request(move |window| {
            // Never hide the only window when no tray watcher can restore it.
            if tray_available.get() && close_to_tray.get() {
                hide_and_hold(window, &hold_guard, &player);
                gtk4::glib::Propagation::Stop
            } else {
                gtk4::glib::Propagation::Proceed
            }
        });
    }

    fn connect_minimize(&self) {
        let tray_available = Rc::clone(&self.tray_available);
        let minimize_to_tray = Rc::clone(&self.minimize_to_tray);
        let hold_guard = Rc::clone(&self.hold_guard);
        let player = Rc::clone(&self.player);
        self.window.connect_realize(move |window| {
            let tray_available = Rc::clone(&tray_available);
            let minimize_to_tray = Rc::clone(&minimize_to_tray);
            let hold_guard = Rc::clone(&hold_guard);
            let player = Rc::clone(&player);
            let window = window.clone();
            let Some(toplevel) = window.surface().and_downcast::<gtk4::gdk::Toplevel>() else {
                return;
            };
            toplevel.connect_state_notify(move |toplevel| {
                let minimized = toplevel
                    .state()
                    .contains(gtk4::gdk::ToplevelState::MINIMIZED);
                if minimized && tray_available.get() && minimize_to_tray.get() {
                    hide_and_hold(&window, &hold_guard, &player);
                }
            });
        });
    }
}

fn hide_and_hold(
    window: &adw::ApplicationWindow,
    hold_guard: &Rc<RefCell<Option<gtk4::gio::ApplicationHoldGuard>>>,
    player: &Player,
) {
    player.pause();
    window.set_visible(false);
    let mut guard = hold_guard.borrow_mut();
    if guard.is_none() {
        *guard = Some(window.application().expect("shell application").hold());
    }
}

#[allow(clippy::too_many_arguments)]
fn make_sink(
    window: &adw::ApplicationWindow,
    application: &adw::Application,
    coordinator: &Rc<RefCell<CoordinatorHandle>>,
    toasts: &adw::ToastOverlay,
    data_dir: &Path,
    config_dir: &Path,
    settings_cell: &Rc<RefCell<Option<Rc<Settings>>>>,
    latest_snapshot: &Rc<RefCell<Option<Arc<AppSnapshot>>>>,
    tray_available: &Rc<Cell<bool>>,
) -> ActionSink {
    let window = window.clone();
    let application = application.clone();
    let coordinator = Rc::clone(coordinator);
    let toasts = toasts.clone();
    // One Busy toast at a time, however many sends bounce off a full queue.
    let busy_shown = Rc::new(Cell::new(false));
    let data_dir: PathBuf = data_dir.to_owned();
    let config_dir: PathBuf = config_dir.to_owned();
    let settings_cell = Rc::clone(settings_cell);
    let latest_snapshot = Rc::clone(latest_snapshot);
    let tray_available = Rc::clone(tray_available);
    // Settings and the test dialog need the sink itself; the cell breaks the
    // construction cycle and is filled right after the closure is built.
    let sink_cell: Rc<RefCell<Option<ActionSink>>> = Rc::new(RefCell::new(None));
    let sink_for_dialogs = Rc::clone(&sink_cell);
    let sink: ActionSink = Rc::new(move |action| {
        let command = match action {
            ShellAction::Command(command) => command,
            ShellAction::Retry => Command::Arm,
            ShellAction::OpenSettings => {
                let sink = sink_for_dialogs.borrow().clone();
                if let Some(sink) = sink {
                    open_settings(
                        &window,
                        sink,
                        &settings_cell,
                        &latest_snapshot,
                        tray_available.get(),
                    );
                }
                return true;
            }
            ShellAction::TestRecording => {
                let sink = sink_for_dialogs.borrow().clone();
                if let Some(sink) = sink {
                    let ready = latest_snapshot.borrow().as_ref().is_some_and(|snapshot| {
                        matches!(
                            snapshot.status,
                            RecorderStatus::Ready | RecorderStatus::WaitingForWow
                        )
                    });
                    present_test_dialog(window.upcast_ref(), sink, ready);
                }
                return true;
            }
            ShellAction::OpenLogs => {
                open_logs(&window, &data_dir, &config_dir);
                return true;
            }
            ShellAction::About => {
                present_about(&window, &application);
                return true;
            }
            ShellAction::CheckForUpdates => {
                present_update_dialog(window.upcast_ref());
                return true;
            }
            ShellAction::Quit => Command::Shutdown,
        };
        let clip = matches!(command, Command::CreateClip(_));
        let sent = coordinator.borrow().send(command);
        if !sent && !busy_shown.replace(true) {
            let toast = adw::Toast::new("The app is busy, try again in a moment.");
            toast.set_timeout(3);
            let busy_shown = Rc::clone(&busy_shown);
            toast.connect_dismissed(move |_| busy_shown.set(false));
            toasts.add_toast(toast);
        } else if sent && clip {
            let toast = adw::Toast::new("Clip queued");
            toast.set_timeout(3);
            toasts.add_toast(toast);
        }
        sent
    });
    *sink_cell.borrow_mut() = Some(Rc::clone(&sink));
    sink
}

/// Open (or re-present) the one Settings dialog against the newest snapshot.
fn open_settings(
    window: &adw::ApplicationWindow,
    sink: ActionSink,
    settings_cell: &Rc<RefCell<Option<Rc<Settings>>>>,
    latest_snapshot: &Rc<RefCell<Option<Arc<AppSnapshot>>>>,
    tray_available: bool,
) {
    if let Some(settings) = settings_cell.borrow().as_ref() {
        settings.dialog.present(Some(window));
        return;
    }
    // The first snapshot arrives within one coordinator tick of startup;
    // until then there is no config to edit.
    let Some(snapshot) = latest_snapshot.borrow().clone() else {
        return;
    };
    let settings = Settings::open(window.upcast_ref(), sink, &snapshot, tray_available);
    {
        let settings_cell = Rc::clone(settings_cell);
        settings.dialog.connect_closed(move |_| {
            settings_cell.borrow_mut().take();
        });
    }
    *settings_cell.borrow_mut() = Some(settings);
}

fn present_about(parent: &impl IsA<gtk4::Widget>, application: &adw::Application) {
    let about = adw::AboutDialog::new();
    about.set_application_name("Warcraft Recorder");
    about.set_application_icon(
        application
            .application_id()
            .as_deref()
            .unwrap_or("warcraft-recorder"),
    );
    about.set_developer_name("JohanWes");
    about.set_version(env!("CARGO_PKG_VERSION"));
    about.set_license_type(gtk4::License::Gpl30);
    about.set_comments(
        "Automatic World of Warcraft recording and review.\n\nCategory icons: \
         Lucide contributors (ISC License); dragon and dungeon icons by \
         Fonticons, Inc. (CC BY 4.0). Full notices are bundled with the \
         application.",
    );
    about.set_website("https://github.com/JohanWes/wow-recorder-linuxwayland");
    about.set_issue_url("https://github.com/JohanWes/wow-recorder-linuxwayland/issues");
    about.present(Some(parent));
}

fn open_logs(parent: &impl IsA<gtk4::Widget>, data_dir: &Path, config_dir: &Path) {
    let parent_window = parent.root().and_downcast::<gtk4::Window>();
    let fallback = config_dir.to_owned();
    let fallback_parent = parent_window.clone();
    let launcher = gtk4::FileLauncher::new(Some(&gtk4::gio::File::for_path(data_dir)));
    launcher.launch(
        parent_window.as_ref(),
        None::<&gtk4::gio::Cancellable>,
        move |result| {
            if result.is_ok() {
                return;
            }
            let launcher = gtk4::FileLauncher::new(Some(&gtk4::gio::File::for_path(&fallback)));
            launcher.launch(
                fallback_parent.as_ref(),
                None::<&gtk4::gio::Cancellable>,
                |result| {
                    if let Err(error) = result {
                        tracing::warn!(%error, "could not open the logs directory");
                    }
                },
            );
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use warcraft_recorder::config::Config;
    use warcraft_recorder::domain::{
        ActivityDetails, Category, Codec, CorrelatedActivity, GameFlavor, LibraryEntry, MediaFacts,
        Outcome, RecorderStatus, RecordingId,
    };

    pub(crate) fn entry(category: Category, title: &str, start_unix_ms: i64) -> LibraryEntry {
        LibraryEntry {
            id: RecordingId::new(),
            media_path: PathBuf::from("/recordings/video.mkv"),
            sidecar_path: PathBuf::from("/recordings/video.json"),
            category,
            flavor: GameFlavor::Retail,
            title: title.to_owned(),
            start_unix_ms,
            duration_ms: 60_000,
            outcome: Outcome::Win,
            protected: false,
            tag: None,
            activity_hash: None,
            player: None,
            combatants: Vec::new(),
            details: ActivityDetails::Manual,
            timeline: Vec::new(),
            media: MediaFacts {
                fps: Some(60),
                width: Some(1920),
                height: Some(1080),
                codec: Some(Codec::H264),
                has_content: true,
            },
        }
    }

    pub(crate) fn snapshot_with(
        status: RecorderStatus,
        config: Config,
        entries: Vec<LibraryEntry>,
    ) -> AppSnapshot {
        let mut category_counts: Vec<(Category, usize)> = Vec::new();
        for entry in &entries {
            match category_counts
                .iter_mut()
                .find(|(category, _)| category == &entry.category)
            {
                Some((_, count)) => *count += 1,
                None => category_counts.push((entry.category.clone(), 1)),
            }
        }
        AppSnapshot {
            entries: Arc::new(entries),
            correlations: Arc::new(Vec::<CorrelatedActivity>::new()),
            category_counts,
            status,
            active: None,
            config,
            setup_problems: Vec::new(),
            advanced_logging: Vec::new(),
            problems: Vec::new(),
            work: None,
            queued_jobs: 0,
            storage_used_bytes: 0,
            protected_over_limit: false,
        }
    }
}
