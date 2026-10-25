// SPDX-License-Identifier: GPL-3.0-or-later

//! Operational controls outside Settings: the Manual-category Start/Stop
//! toolbar, the test-recording category chooser, the capture-reselection
//! explanation, and the post-update "What's new" dialog.
//!
//! The legacy manual-recording sound assets are not redistributable, so the
//! retained `manual.sound` setting rings the display bell instead.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use warcraft_recorder::coordinator::{AppSnapshot, Command};
use warcraft_recorder::domain::{Category, RecorderStatus};
use warcraft_recorder::storage::now_unix_ms;

use super::status::ElapsedLabel;
use super::{ActionSink, ShellAction, TEST_CATEGORIES, category_label};

/// What the Manual toolbar shows, derived from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualView {
    /// The bar exists only in the Manual category with manual recording on.
    pub visible: bool,
    /// Start is possible only when the recorder is armed and idle.
    pub start_enabled: bool,
    /// Stop replaces Start while the manual recording runs.
    pub stop_visible: bool,
    pub elapsed_anchor_ms: Option<i64>,
}

pub fn manual_view(snapshot: &AppSnapshot) -> ManualView {
    let manual_active = matches!(
        snapshot.status,
        RecorderStatus::Recording { manual: true, .. }
    );
    ManualView {
        visible: snapshot.config.interface.selected_category == Category::Manual
            && snapshot.config.manual.enabled,
        // Starting arms the recorder when WoW is not running.
        start_enabled: matches!(
            snapshot.status,
            RecorderStatus::Ready | RecorderStatus::WaitingForWow
        ),
        stop_visible: manual_active,
        elapsed_anchor_ms: match snapshot.status {
            RecorderStatus::Recording {
                manual: true,
                started_unix_ms,
                ..
            } => Some(started_unix_ms),
            _ => None,
        },
    }
}

/// Explanation shown in the test-recording chooser, matching the injected
/// 5 s (raids 20 s) synthetic activities.
pub const TEST_EXPLANATION: &str = "Runs a short synthetic activity to verify capture and \
    saving. Most categories record for about 5 seconds; raids record for about 20. The result \
    appears in the library like a real recording. Force end stops it early.";

/// The Manual category toolbar. Sounds follow `manual.sound` using the
/// display bell on start/stop/failed-start transitions.
pub struct ManualBar {
    pub widget: gtk4::Box,
    start: gtk4::Button,
    stop: gtk4::Button,
    elapsed: ElapsedLabel,
    was_active: Cell<bool>,
    /// Wall-clock time of the last Start click, to catch the coordinator's
    /// "could not be started" problem for the error bell.
    start_requested_ms: Rc<Cell<Option<i64>>>,
}

impl ManualBar {
    pub fn new(sink: ActionSink) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        widget.set_margin_top(6);
        widget.set_margin_bottom(6);
        widget.set_margin_start(12);
        widget.set_margin_end(12);
        widget.set_visible(false);

        let start = gtk4::Button::with_label("Start recording");
        start.add_css_class("suggested-action");
        start.set_tooltip_text(Some("Start a manual recording"));
        let stop = gtk4::Button::with_label("Stop recording");
        stop.add_css_class("destructive-action");
        stop.set_tooltip_text(Some("Stop the manual recording"));
        stop.set_visible(false);
        let elapsed = ElapsedLabel::new("Elapsed manual recording time");

        widget.append(&start);
        widget.append(&stop);
        widget.append(&elapsed.label);

        let start_requested_ms: Rc<Cell<Option<i64>>> = Rc::new(Cell::new(None));
        let bar = Self {
            widget,
            start: start.clone(),
            stop: stop.clone(),
            elapsed,
            was_active: Cell::new(false),
            start_requested_ms: Rc::clone(&start_requested_ms),
        };

        {
            let sink = Rc::clone(&sink);
            let requested = start_requested_ms;
            start.connect_clicked(move |_| {
                if sink(ShellAction::Command(Command::StartManual)) {
                    requested.set(Some(now_unix_ms()));
                }
            });
        }
        stop.connect_clicked(move |_| {
            sink(ShellAction::Command(Command::StopManual));
        });
        bar
    }

    pub fn apply(&self, snapshot: &AppSnapshot) {
        let view = manual_view(snapshot);
        self.widget.set_visible(view.visible);
        self.start.set_visible(!view.stop_visible);
        self.start.set_sensitive(view.start_enabled);
        self.stop.set_visible(view.stop_visible);
        self.elapsed.set_anchor(view.elapsed_anchor_ms);

        // Sound transitions: bell on start/stop, and on a failed start
        // reported by the coordinator after our request.
        let sounds = snapshot.config.manual.sound;
        if view.stop_visible != self.was_active.get() {
            self.was_active.set(view.stop_visible);
            self.start_requested_ms.set(None);
            if sounds {
                bell();
            }
        } else if let Some(requested_ms) = self.start_requested_ms.get()
            && snapshot.problems.iter().any(|problem| {
                problem.occurred_unix_ms >= requested_ms
                    && problem.summary == "A manual recording could not be started."
            })
        {
            self.start_requested_ms.set(None);
            if sounds {
                bell();
            }
        }
    }
}

/// The bell is per display, not per widget.
fn bell() {
    if let Some(display) = gtk4::gdk::Display::default() {
        display.beep();
    }
}

/// The window-menu/Settings test-recording chooser: category list, duration
/// explanation, and one Start that sends `RunTest`.
pub fn present_test_dialog(parent: &gtk4::Widget, sink: ActionSink, ready: bool) {
    let dialog = adw::AlertDialog::new(Some("Test recording"), Some(TEST_EXPLANATION));
    let labels: Vec<&str> = TEST_CATEGORIES.iter().map(category_label).collect();
    let combo = gtk4::DropDown::from_strings(&labels);
    combo.set_tooltip_text(Some("Test recording category"));
    dialog.set_extra_child(Some(&combo));
    dialog.add_responses(&[("cancel", "Cancel"), ("start", "Start test")]);
    dialog.set_response_appearance("start", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("start"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("start", ready);
    if !ready {
        dialog.set_body(&format!(
            "{TEST_EXPLANATION}\n\nThe recorder is not ready: a test needs an armed, idle capture."
        ));
    }
    dialog.connect_response(Some("start"), move |_, _| {
        if let Some(category) = TEST_CATEGORIES.get(combo.selected() as usize) {
            sink(ShellAction::Command(Command::RunTest {
                category: category.clone(),
            }));
        }
    });
    dialog.present(Some(parent));
}

/// The published install script: adds the signed Flatpak remote and installs
/// or updates the app.
const UPDATE_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/JohanWes/wow-recorder-linuxwayland/main/install.sh";

fn flatpak_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| path.join("flatpak").is_file())
    })
}

fn info_dialog(parent: &gtk4::Widget, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("close", "Close")]);
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

/// "Check for updates": inside Flatpak, updates arrive with the system's own
/// app updates; without Flatpak, say what is missing; otherwise confirm and
/// run the install script off the GTK thread.
pub fn present_update_dialog(parent: &gtk4::Widget) {
    if cfg!(windows) {
        info_dialog(
            parent,
            "Check the releases page",
            "Download the latest Windows installer from \
             github.com/JohanWes/wow-recorder-linuxwayland/releases.",
        );
        return;
    }
    if std::path::Path::new("/.flatpak-info").exists() {
        info_dialog(
            parent,
            "Updates are automatic",
            "New versions of Warcraft Recorder arrive with your usual system app updates.",
        );
        return;
    }
    if !flatpak_available() {
        info_dialog(
            parent,
            "Flatpak is needed to update",
            "Warcraft Recorder is distributed as a Flatpak. Install Flatpak from your \
             distribution, then check for updates again.",
        );
        return;
    }
    let dialog = adw::AlertDialog::new(
        Some("Update Warcraft Recorder?"),
        Some("This installs the latest version. It can take a few minutes."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("update", "Update")]);
    dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("update"));
    dialog.set_close_response("cancel");
    let parent = parent.clone();
    let dialog_parent = parent.clone();
    dialog.connect_response(Some("update"), move |_, _| run_update(&parent));
    dialog.present(Some(&dialog_parent));
}

/// Runs the install script on GIO's blocking pool: a copy next to the
/// executable if present (development), otherwise the published one.
fn run_update(parent: &gtk4::Widget) {
    let parent = parent.clone();
    gtk4::glib::spawn_future_local(async move {
        let output = gtk4::gio::spawn_blocking(|| {
            let local_script = std::env::current_exe()
                .ok()
                .and_then(|exe| Some(exe.parent()?.join("install.sh")))
                .filter(|script| script.is_file());
            let mut command = std::process::Command::new("bash");
            match local_script {
                Some(script) => {
                    command.arg(script);
                }
                None => {
                    command
                        .arg("-c")
                        .arg(format!("curl -fsSL {UPDATE_SCRIPT_URL} | bash"));
                }
            }
            command.stdin(std::process::Stdio::null()).output()
        })
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("the update task crashed")));
        match output {
            Ok(output) if output.status.success() => info_dialog(
                &parent,
                "Update installed",
                "The new version is installed and starting. You can close this one.",
            ),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                info_dialog(
                    &parent,
                    "The update did not finish",
                    &format!(
                        "Warcraft Recorder was not updated.\n\n{}",
                        if tail.is_empty() {
                            "No reason was reported.".to_owned()
                        } else {
                            tail.join("\n")
                        }
                    ),
                );
            }
            Err(error) => info_dialog(
                &parent,
                "The update could not start",
                &format!("Warcraft Recorder could not run the update: {error}"),
            ),
        }
    });
}

/// The capture-reselection explanation: the portal prompt follows, and
/// cancelling it keeps the previous usable selection.
pub fn present_reselect_dialog(parent: &gtk4::Widget, sink: ActionSink) {
    let dialog = adw::AlertDialog::new(
        Some("Reselect capture target"),
        Some(
            "Your desktop will show its screen-share prompt to pick the monitor or window to \
             record. Cancelling the prompt keeps the current selection.",
        ),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("reselect", "Reselect")]);
    dialog.set_response_appearance("reselect", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("reselect"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("reselect"), move |_, _| {
        sink(ShellAction::Command(Command::ReselectCaptureTarget));
    });
    dialog.present(Some(parent));
}

/// The commit subjects of every release, generated by
/// `scripts/generate-release-notes.sh` and compiled in: neither the Flatpak
/// build tree nor the installed app has a repository to read a log from.
const RELEASE_NOTES: &str = include_str!("../../../data/release-notes.md");

/// The commit subjects recorded for `version`, oldest first. Empty for a
/// version with no section, which is every locally built one.
pub fn release_notes(version: &str) -> Vec<&'static str> {
    notes_for(RELEASE_NOTES, version)
}

fn notes_for<'a>(notes: &'a str, version: &str) -> Vec<&'a str> {
    notes
        .lines()
        .skip_while(|line| {
            line.strip_prefix("## ")
                .is_none_or(|heading| heading.trim() != version)
        })
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("- "))
        .map(str::trim_end)
        .collect()
}

/// Shown once per updated version: what landed between the previous release
/// and this one, one commit per row. Closing it by any route acknowledges it.
pub fn present_release_notes(parent: &gtk4::Widget, sink: ActionSink, notes: &[&str]) {
    let list = gtk4::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk4::SelectionMode::None);
    for note in notes {
        let row = adw::ActionRow::new();
        // Commit subjects are arbitrary text, not Pango markup.
        row.set_use_markup(false);
        row.set_title(note);
        row.set_title_lines(0);
        list.append(&row);
    }

    let intro = gtk4::Label::new(Some("Changes in this update"));
    intro.set_xalign(0.0);
    intro.add_css_class("dim-label");
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.set_margin_top(6);
    body.set_margin_bottom(18);
    body.append(&intro);
    body.append(&list);

    // The dialog keeps its natural height until the list outgrows a screenful.
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(420);
    scroller.set_child(Some(&body));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "What's new",
        &format!("Version {}", warcraft_recorder::VERSION),
    )));
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(true);

    let ok = gtk4::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    actions.set_halign(gtk4::Align::Center);
    actions.set_margin_top(8);
    actions.set_margin_bottom(8);
    actions.append(&ok);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.add_bottom_bar(&actions);
    toolbar_view.set_content(Some(&scroller));

    let dialog = adw::Dialog::new();
    dialog.set_title("What's new");
    dialog.set_content_width(520);
    dialog.set_child(Some(&toolbar_view));
    {
        let dialog = dialog.clone();
        ok.connect_clicked(move |_| {
            dialog.close();
        });
    }
    dialog.connect_closed(move |_| {
        sink(ShellAction::Command(Command::DismissReleaseNotes));
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_recorder::config::Config;

    fn snapshot(status: RecorderStatus, selected: Category, manual: bool) -> AppSnapshot {
        let mut config = Config::default();
        config.interface.selected_category = selected;
        config.manual.enabled = manual;
        crate::ui::window::tests::snapshot_with(status, config, Vec::new())
    }

    #[test]
    fn manual_bar_is_gated_and_shows_stop_only_for_a_manual_recording() {
        // Idle: Start in the Manual category, only with manual recording on.
        let idle = manual_view(&snapshot(RecorderStatus::Ready, Category::Manual, true));
        assert!(idle.visible && idle.start_enabled && !idle.stop_visible);
        assert!(!manual_view(&snapshot(RecorderStatus::Ready, Category::Manual, false)).visible);

        let manual = RecorderStatus::Recording {
            category: Category::Manual,
            title: "Manual recording".to_owned(),
            started_unix_ms: 42,
            manual: true,
            test: false,
        };
        let view = manual_view(&snapshot(manual, Category::Manual, true));
        assert!(view.visible && view.stop_visible);
        assert!(!view.start_enabled);
        assert_eq!(view.elapsed_anchor_ms, Some(42));

        // An automatic recording in progress never shows manual Stop.
        let automatic = RecorderStatus::Recording {
            category: Category::Raids,
            title: "Boss".to_owned(),
            started_unix_ms: 42,
            manual: false,
            test: false,
        };
        let view = manual_view(&snapshot(automatic, Category::Manual, true));
        assert!(!view.stop_visible);
        assert!(!view.start_enabled);
        assert_eq!(view.elapsed_anchor_ms, None);
    }

    #[test]
    fn release_notes_take_the_requested_version_and_stop_at_the_next_one() {
        let generated = "# Release notes\n\nPreamble.\n\n## 1.2.0\n- Newest change\n- \
                         Second change  \n\n## 1.1.0\n- Older change\n";
        assert_eq!(
            notes_for(generated, "1.2.0"),
            ["Newest change", "Second change"]
        );
        assert_eq!(notes_for(generated, "1.1.0"), ["Older change"]);
        assert!(notes_for(generated, "9.9.9").is_empty());
        // The shipped file must stay parseable even before a release adds a
        // section for the version being built.
        assert!(release_notes("0.0.0").is_empty());
    }
}
