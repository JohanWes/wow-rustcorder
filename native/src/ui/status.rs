// SPDX-License-Identifier: GPL-3.0-or-later

//! The sidebar status card: recorder state, elapsed time, Force end,
//! per-flavour advanced-combat-logging warnings, and the bounded recovered
//! problem list. The Linux recorder never reports microphone state, so there
//! is no microphone badge.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use warcraft_rustcorder::config::Config;
use warcraft_rustcorder::coordinator::AppSnapshot;
use warcraft_rustcorder::domain::{Problem, RecorderStatus, RecoveryAction};
use warcraft_rustcorder::storage::now_unix_ms;

use super::{ActionSink, ShellAction, clear_box, format_clock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ready,
    Recording,
    Waiting,
    Overrunning,
    Finalizing,
    Invalid,
}

impl Tone {
    fn css_class(self) -> &'static str {
        match self {
            Self::Ready => "tone-ready",
            Self::Recording => "tone-recording",
            Self::Waiting => "tone-waiting",
            Self::Overrunning => "tone-overrunning",
            Self::Finalizing => "tone-finalizing",
            Self::Invalid => "tone-invalid",
        }
    }
}

/// What the card shows, derived from one snapshot. `elapsed_anchor_ms` is the
/// wall-clock anchor the card's own one-second timeout renders from; the
/// coordinator never publishes per-second snapshots for this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusView {
    pub title: String,
    pub detail: String,
    pub tone: Tone,
    pub elapsed_anchor_ms: Option<i64>,
    pub show_force_end: bool,
    pub show_spinner: bool,
}

/// Enabled flavour labels in config order, for the Ready detail line.
pub fn enabled_flavors(config: &Config) -> Vec<&'static str> {
    let mut flavors = Vec::new();
    if config.flavors.retail.enabled {
        flavors.push("Retail");
    }
    if config.flavors.retail_ptr.enabled {
        flavors.push("Retail PTR");
    }
    if config.flavors.classic.enabled {
        flavors.push("Classic");
    }
    if config.flavors.classic_ptr.enabled {
        flavors.push("Classic PTR");
    }
    if config.flavors.era.enabled {
        flavors.push("Era");
    }
    flavors
}

/// The `advanced_logging` snapshot field carries short field names; these
/// are the per-flavour warning rows the card renders. Only a `Config.wtf`
/// that was read and says "off" warrants a warning; an unreadable one is
/// unknown, not off, and must stay silent.
pub fn advanced_logging_warnings(snapshot: &AppSnapshot) -> Vec<String> {
    snapshot
        .advanced_logging
        .iter()
        .filter(|(_, enabled)| *enabled == Some(false))
        .map(|(field, _)| {
            let flavor = match *field {
                "retail" => "Retail",
                "retail_ptr" => "Retail PTR",
                "classic" => "Classic",
                "classic_ptr" => "Classic PTR",
                "era" => "Era",
                other => other,
            };
            format!("Advanced combat logging is off for {flavor}.")
        })
        .collect()
}

pub fn view(snapshot: &AppSnapshot) -> StatusView {
    match &snapshot.status {
        RecorderStatus::SetupRequired => StatusView {
            title: "Setup required".to_owned(),
            detail: snapshot
                .setup_problems
                .first()
                .map(|problem| problem.message.clone())
                .unwrap_or_else(|| "Finish setup in Settings.".to_owned()),
            tone: Tone::Invalid,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::WaitingForWow => StatusView {
            title: "Waiting".to_owned(),
            detail: "Waiting for World of Warcraft to start.".to_owned(),
            tone: Tone::Waiting,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Ready => StatusView {
            title: "Ready".to_owned(),
            detail: format!(
                "Watching combat logs: {}.",
                enabled_flavors(&snapshot.config).join(", ")
            ),
            tone: Tone::Ready,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Recording {
            title,
            started_unix_ms,
            manual,
            test,
            ..
        } => StatusView {
            title: if *manual {
                "Manual recording".to_owned()
            } else if *test {
                "Test recording".to_owned()
            } else {
                "Recording".to_owned()
            },
            detail: title.clone(),
            tone: Tone::Recording,
            elapsed_anchor_ms: Some(*started_unix_ms),
            // Force end is automatic-only; manual recordings get their own Stop.
            show_force_end: !manual,
            show_spinner: false,
        },
        RecorderStatus::Overrunning {
            title,
            started_unix_ms,
        } => StatusView {
            title: "Overrunning".to_owned(),
            detail: format!("{title}: recording a few seconds of aftermath."),
            tone: Tone::Overrunning,
            elapsed_anchor_ms: Some(*started_unix_ms),
            show_force_end: false,
            show_spinner: false,
        },
        RecorderStatus::Finalizing { title } => StatusView {
            title: "Saving".to_owned(),
            detail: match &snapshot.work {
                Some(work) => match work.total {
                    Some(total) if total > 0 => {
                        format!("{title}: {}%", work.completed.saturating_mul(100) / total)
                    }
                    _ => title.clone(),
                },
                None => title.clone(),
            },
            tone: Tone::Finalizing,
            elapsed_anchor_ms: None,
            show_force_end: false,
            show_spinner: true,
        },
    }
}

/// Format the elapsed anchor against `now_unix_ms`; a now-before-anchor
/// snapshot renders as zero rather than a negative time.
pub fn elapsed_label(anchor_unix_ms: i64, now_unix_ms: i64) -> String {
    format_clock(now_unix_ms.saturating_sub(anchor_unix_ms).max(0) as u64)
}

pub(crate) fn recovery_label(action: RecoveryAction) -> &'static str {
    match action {
        RecoveryAction::OpenSettings => "Open Settings",
        RecoveryAction::ReselectCaptureTarget => "Reselect capture target",
        RecoveryAction::Retry => "Try again",
        RecoveryAction::OpenLogs => "Open logs",
        RecoveryAction::Quit => "Quit",
    }
}

pub(crate) fn shell_action(action: RecoveryAction) -> ShellAction {
    match action {
        RecoveryAction::OpenSettings => ShellAction::OpenSettings,
        RecoveryAction::ReselectCaptureTarget => {
            ShellAction::Command(warcraft_rustcorder::coordinator::Command::ReselectCaptureTarget)
        }
        RecoveryAction::Retry => ShellAction::Retry,
        RecoveryAction::OpenLogs => ShellAction::OpenLogs,
        RecoveryAction::Quit => ShellAction::Quit,
    }
}

/// A label counting up from a wall-clock anchor. It renders itself once a
/// second, and only while it has an anchor and is on screen, so a window
/// hidden to the tray costs no wakeups; the coordinator never publishes
/// per-second snapshots for this.
pub struct ElapsedLabel {
    pub label: gtk4::Label,
    anchor: Rc<Cell<Option<i64>>>,
    running: Rc<Cell<bool>>,
}

impl ElapsedLabel {
    pub fn new(tooltip: &str) -> Self {
        let label = gtk4::Label::new(None);
        label.add_css_class("monospace");
        label.set_tooltip_text(Some(tooltip));
        label.set_visible(false);
        let anchor: Rc<Cell<Option<i64>>> = Rc::default();
        let running: Rc<Cell<bool>> = Rc::default();
        {
            let anchor = Rc::clone(&anchor);
            let running = Rc::clone(&running);
            label.connect_map(move |label| tick(label, &anchor, &running));
        }
        Self {
            label,
            anchor,
            running,
        }
    }

    pub fn set_anchor(&self, anchor: Option<i64>) {
        self.anchor.set(anchor);
        self.label.set_visible(anchor.is_some());
        tick(&self.label, &self.anchor, &self.running);
    }
}

/// Render the label now and keep one one-second timeout rendering it until
/// the anchor goes away or the label is unmapped; mapping restarts it.
fn tick(label: &gtk4::Label, anchor: &Rc<Cell<Option<i64>>>, running: &Rc<Cell<bool>>) {
    let Some(anchor_ms) = anchor.get() else {
        return;
    };
    label.set_label(&elapsed_label(anchor_ms, now_unix_ms()));
    if !label.is_mapped() || running.replace(true) {
        return;
    }
    let anchor = Rc::clone(anchor);
    let running = Rc::clone(running);
    let label = label.downgrade();
    gtk4::glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        match (label.upgrade(), anchor.get()) {
            (Some(label), Some(anchor_ms)) if label.is_mapped() => {
                label.set_label(&elapsed_label(anchor_ms, now_unix_ms()));
                gtk4::glib::ControlFlow::Continue
            }
            _ => {
                running.set(false);
                gtk4::glib::ControlFlow::Break
            }
        }
    });
}

/// The status card widget. All behavior arrives through `apply`; all outgoing
/// intent goes through the one `ActionSink`.
pub struct StatusCard {
    pub widget: gtk4::Box,
    light: gtk4::Box,
    title: gtk4::Label,
    elapsed: ElapsedLabel,
    spinner: libadwaita::Spinner,
    detail: gtk4::Label,
    force_end: gtk4::Button,
    warnings: gtk4::Box,
    problems_expander: gtk4::Expander,
    problems: gtk4::Box,
    tray_note: gtk4::Label,
    sink: ActionSink,
    /// What the two rebuilt sections were last built from. Rebuilding them per
    /// snapshot throws away GTK objects, relayouts the rail, and collapses any
    /// problem row the user had expanded.
    rendered_warnings: RefCell<Vec<String>>,
    rendered_problems: RefCell<Vec<Problem>>,
    rendered_tone: Cell<Option<Tone>>,
}

impl StatusCard {
    pub fn new(sink: ActionSink) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        widget.add_css_class("card");
        widget.add_css_class("wr-status-card");
        widget.set_margin_top(12);
        widget.set_margin_bottom(12);
        widget.set_margin_start(12);
        widget.set_margin_end(12);

        let light = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        light.add_css_class("status-light");
        light.add_css_class("tone-waiting");
        light.set_valign(gtk4::Align::Center);
        light.set_tooltip_text(Some("Recorder status"));

        let title = gtk4::Label::new(Some("Starting"));
        title.add_css_class("heading");
        title.set_hexpand(true);
        title.set_xalign(0.0);

        let elapsed = ElapsedLabel::new("Elapsed recording time");

        let spinner = libadwaita::Spinner::new();
        spinner.set_visible(false);

        let title_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        title_row.append(&light);
        title_row.append(&title);
        title_row.append(&elapsed.label);
        title_row.append(&spinner);

        let detail = gtk4::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        detail.add_css_class("dim-label");
        detail.add_css_class("caption");

        let force_end = gtk4::Button::with_label("Force end");
        force_end.add_css_class("destructive-action");
        force_end.set_halign(gtk4::Align::End);
        force_end.set_tooltip_text(Some(
            "End the current recording now. Normally this is not required.",
        ));
        force_end.set_visible(false);
        {
            let sink = Rc::clone(&sink);
            let force_end_widget = force_end.clone();
            force_end.connect_clicked(move |_| {
                if !sink(ShellAction::Command(
                    warcraft_rustcorder::coordinator::Command::ForceEnd,
                )) {
                    force_end_widget.set_sensitive(false);
                    let force_end_widget = force_end_widget.clone();
                    gtk4::glib::timeout_add_local_once(
                        std::time::Duration::from_secs(2),
                        move || force_end_widget.set_sensitive(true),
                    );
                }
            });
        }

        let warnings = gtk4::Box::new(gtk4::Orientation::Vertical, 2);

        let problems = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        let problems_expander = gtk4::Expander::new(Some("Problems"));
        problems_expander.set_child(Some(&problems));
        problems_expander.set_visible(false);

        let tray_note = gtk4::Label::new(Some(
            "No system tray found: closing the window quits Warcraft Rustcorder.",
        ));
        tray_note.set_xalign(0.0);
        tray_note.set_wrap(true);
        tray_note.add_css_class("dim-label");
        tray_note.add_css_class("caption");
        tray_note.set_visible(false);

        widget.append(&title_row);
        widget.append(&detail);
        widget.append(&force_end);
        widget.append(&warnings);
        widget.append(&problems_expander);
        widget.append(&tray_note);

        Self {
            widget,
            light,
            title,
            elapsed,
            spinner,
            detail,
            force_end,
            warnings,
            problems_expander,
            problems,
            tray_note,
            sink,
            rendered_warnings: RefCell::new(Vec::new()),
            rendered_problems: RefCell::new(Vec::new()),
            rendered_tone: Cell::new(None),
        }
    }

    pub fn apply(&self, snapshot: &AppSnapshot) {
        let view = view(snapshot);

        if self.rendered_tone.replace(Some(view.tone)) != Some(view.tone) {
            for class in [
                "tone-ready",
                "tone-recording",
                "tone-waiting",
                "tone-overrunning",
                "tone-finalizing",
                "tone-invalid",
            ] {
                self.light.remove_css_class(class);
            }
            self.light.add_css_class(view.tone.css_class());
        }

        self.title.set_label(&view.title);
        self.detail.set_label(&view.detail);
        self.detail.set_visible(!view.detail.is_empty());
        self.spinner.set_visible(view.show_spinner);
        self.force_end.set_visible(view.show_force_end);

        self.elapsed.set_anchor(view.elapsed_anchor_ms);

        let warnings = advanced_logging_warnings(snapshot);
        if *self.rendered_warnings.borrow() != warnings {
            clear_box(&self.warnings);
            for warning in &warnings {
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
                row.set_tooltip_text(Some(
                    "Enable advanced combat logging in-game (System → Network) or some stats will be missing.",
                ));
                let icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
                icon.add_css_class("warning");
                let label = gtk4::Label::new(Some(warning));
                label.set_xalign(0.0);
                label.set_wrap(true);
                label.add_css_class("caption");
                label.add_css_class("dim-label");
                row.append(&icon);
                row.append(&label);
                self.warnings.append(&row);
            }
            *self.rendered_warnings.borrow_mut() = warnings;
        }

        self.apply_problems(&snapshot.problems);
    }

    /// The bounded recovered-problem list: one expandable row per problem
    /// with its technical detail and one recovery action. Unchanged lists keep
    /// their rows, so an expanded problem stays expanded.
    fn apply_problems(&self, problems: &[Problem]) {
        if *self.rendered_problems.borrow() == problems {
            return;
        }
        self.rendered_problems.borrow_mut().clear();
        self.rendered_problems
            .borrow_mut()
            .extend_from_slice(problems);
        self.problems_expander.set_visible(!problems.is_empty());
        self.problems_expander
            .set_label(Some(&format!("Problems ({})", problems.len())));
        clear_box(&self.problems);
        for problem in problems {
            let row = gtk4::Expander::new(Some(&problem.summary));
            let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
            if let Some(detail) = &problem.safe_detail {
                let detail_label = gtk4::Label::new(Some(detail));
                detail_label.add_css_class("problem-detail");
                detail_label.set_selectable(true);
                detail_label.set_wrap(true);
                detail_label.set_xalign(0.0);
                body.append(&detail_label);
            }
            if let Some(action) = problem.recovery_action {
                let button = gtk4::Button::with_label(recovery_label(action));
                button.set_halign(gtk4::Align::End);
                let sink = Rc::clone(&self.sink);
                button.connect_clicked(move |_| {
                    sink(shell_action(action));
                });
                body.append(&button);
            }
            row.set_child(Some(&body));
            self.problems.append(&row);
        }
    }

    pub fn set_tray_available(&self, available: bool) {
        self.tray_note.set_visible(!available);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advanced_logging_warns_only_when_config_wtf_says_off() {
        let mut snapshot = crate::ui::window::tests::snapshot_with(
            RecorderStatus::Ready,
            Config::default(),
            Vec::new(),
        );
        // Read and off, read and on, and unreadable: only the first warns.
        snapshot.advanced_logging = vec![
            ("retail", Some(false)),
            ("classic", Some(true)),
            ("era", None),
        ];
        let warnings = advanced_logging_warnings(&snapshot);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Retail"));
    }

    #[test]
    fn elapsed_format_stays_compact() {
        assert_eq!(elapsed_label(1_000, 61_500), "1:00");
        assert_eq!(elapsed_label(0, 3_661_000), "1:01:01");
        assert_eq!(elapsed_label(5_000, 1_000), "0:00");
    }
}
