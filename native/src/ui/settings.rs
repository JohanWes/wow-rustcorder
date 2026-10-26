// SPDX-License-Identifier: GPL-3.0-or-later

//! The Settings dialog: four preference pages (Capture, Audio, Activities,
//! Storage & interface) built from the spec tables below, editing one draft
//! `Config` that is validated and sent as `SaveConfig` on Apply. libadwaita's
//! `AdwPreferencesDialog` exposes no Apply/Cancel actions, so this is one
//! `AdwDialog` holding an `AdwViewSwitcher` over four `AdwPreferencesPage`s.
//!
//! Folder selection uses `GtkFileDialog::select_folder`; access probes run on
//! GIO's blocking pool, never the GTK thread. Audio devices come from
//! `Recorder::audio_devices` the same way.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use warcraft_recorder::config::{AuthorizedPath, Config, ValidationProblem};
use warcraft_recorder::coordinator::{AppSnapshot, Command};
use warcraft_recorder::domain::{
    Codec, RaidDifficulty, RecorderStatus, ReplayStorage, StorageLimit,
};
use warcraft_recorder::recorder::{AudioDevice, Recorder};
use warcraft_recorder::storage::now_unix_ms;

use super::operational_actions::present_reselect_dialog;
use super::{ActionSink, ShellAction};

// --- Field specs: the one table mapping retained config fields to rows ---

pub struct SpinSpec {
    pub field: &'static str,
    pub title: &'static str,
    /// Units and bounds, matching config validation.
    pub subtitle: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub get: fn(&Config) -> f64,
    pub set: fn(&mut Config, f64),
}

pub struct SwitchSpec {
    pub field: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    pub get: fn(&Config) -> bool,
    pub set: fn(&mut Config, bool),
}

pub struct ComboSpec {
    pub field: &'static str,
    pub title: &'static str,
    pub choices: &'static [&'static str],
    pub get: fn(&Config) -> u32,
    pub set: fn(&mut Config, u32),
}

/// A directory field with its chooser probe requirement and, for the game
/// flavours, the enabled toggle rendered on the same row.
pub struct PathSpec {
    pub field: &'static str,
    pub title: &'static str,
    pub needs_write: bool,
    pub get: fn(&Config) -> &AuthorizedPath,
    pub set: fn(&mut Config, AuthorizedPath),
    pub enabled: Option<EnabledAccess>,
}

/// The flavour-enabled toggle rendered on a path row.
pub struct EnabledAccess {
    pub get: fn(&Config) -> bool,
    pub set: fn(&mut Config, bool),
}

pub static CAPTURE_COMBOS: [ComboSpec; 2] = [
    ComboSpec {
        field: "capture.codec",
        title: "Video codec",
        choices: &["H.264", "HEVC", "AV1"],
        get: |config| match config.capture.codec {
            Codec::H264 => 0,
            Codec::Hevc => 1,
            Codec::Av1 => 2,
        },
        set: |config, index| {
            config.capture.codec = match index {
                1 => Codec::Hevc,
                2 => Codec::Av1,
                _ => Codec::H264,
            }
        },
    },
    ComboSpec {
        field: "capture.replay_storage",
        title: "Replay buffer storage",
        choices: &["RAM", "Disk"],
        get: |config| match config.capture.replay_storage {
            ReplayStorage::Ram => 0,
            ReplayStorage::Disk => 1,
        },
        set: |config, index| {
            config.capture.replay_storage = if index == 1 {
                ReplayStorage::Disk
            } else {
                ReplayStorage::Ram
            }
        },
    },
];

pub static CAPTURE_SPINS: [SpinSpec; 4] = [
    SpinSpec {
        field: "capture.fps",
        title: "FPS",
        subtitle: "Frames per second (15–60)",
        min: 15.0,
        max: 60.0,
        step: 1.0,
        get: |config| f64::from(config.capture.fps),
        set: |config, value| config.capture.fps = value as u32,
    },
    SpinSpec {
        field: "capture.bitrate_kbps",
        title: "Bitrate",
        subtitle: "Kbps (1000–200000)",
        min: 1_000.0,
        max: 200_000.0,
        step: 500.0,
        get: |config| f64::from(config.capture.bitrate_kbps),
        set: |config, value| config.capture.bitrate_kbps = value as u32,
    },
    SpinSpec {
        field: "capture.replay_buffer_seconds",
        title: "Replay buffer",
        subtitle: "Seconds of lead-in kept before an activity is detected (30–600)",
        min: 30.0,
        max: 600.0,
        step: 10.0,
        get: |config| f64::from(config.capture.replay_buffer_seconds),
        set: |config, value| config.capture.replay_buffer_seconds = value as u32,
    },
    SpinSpec {
        field: "capture.extra_lead_in_seconds",
        title: "Extra lead-in",
        subtitle: "Seconds added to the measured detection delay (0–30)",
        min: 0.0,
        max: 30.0,
        step: 1.0,
        get: |config| f64::from(config.capture.extra_lead_in_seconds),
        set: |config, value| config.capture.extra_lead_in_seconds = value as u32,
    },
];

pub static CAPTURE_SWITCHES: [SwitchSpec; 1] = [SwitchSpec {
    field: "capture.capture_cursor",
    title: "Capture cursor",
    subtitle: "",
    get: |config| config.capture.capture_cursor,
    set: |config, value| config.capture.capture_cursor = value,
}];

/// Rail order: raids, dungeons, arena sizes, skirmish, shuffle, battlegrounds,
/// challenge modes.
pub static ACTIVITY_SWITCHES: [SwitchSpec; 9] = [
    SwitchSpec {
        field: "activities.record_raids",
        title: "Record raids",
        subtitle: "",
        get: |config| config.activities.record_raids,
        set: |config, value| config.activities.record_raids = value,
    },
    SwitchSpec {
        field: "activities.record_dungeons",
        title: "Record Mythic+ dungeons",
        subtitle: "",
        get: |config| config.activities.record_dungeons,
        set: |config, value| config.activities.record_dungeons = value,
    },
    SwitchSpec {
        field: "activities.record_two_v_two",
        title: "Record 2v2",
        subtitle: "",
        get: |config| config.activities.record_two_v_two,
        set: |config, value| config.activities.record_two_v_two = value,
    },
    SwitchSpec {
        field: "activities.record_three_v_three",
        title: "Record 3v3",
        subtitle: "",
        get: |config| config.activities.record_three_v_three,
        set: |config, value| config.activities.record_three_v_three = value,
    },
    SwitchSpec {
        field: "activities.record_five_v_five",
        title: "Record 5v5",
        subtitle: "",
        get: |config| config.activities.record_five_v_five,
        set: |config, value| config.activities.record_five_v_five = value,
    },
    SwitchSpec {
        field: "activities.record_skirmish",
        title: "Record skirmishes",
        subtitle: "",
        get: |config| config.activities.record_skirmish,
        set: |config, value| config.activities.record_skirmish = value,
    },
    SwitchSpec {
        field: "activities.record_solo_shuffle",
        title: "Record Solo Shuffle",
        subtitle: "",
        get: |config| config.activities.record_solo_shuffle,
        set: |config, value| config.activities.record_solo_shuffle = value,
    },
    SwitchSpec {
        field: "activities.record_battlegrounds",
        title: "Record battlegrounds",
        subtitle: "",
        get: |config| config.activities.record_battlegrounds,
        set: |config, value| config.activities.record_battlegrounds = value,
    },
    SwitchSpec {
        field: "activities.record_challenge_modes",
        title: "Record challenge modes",
        subtitle: "Classic challenge-mode dungeons",
        get: |config| config.activities.record_challenge_modes,
        set: |config, value| config.activities.record_challenge_modes = value,
    },
];

pub static ACTIVITY_COMBOS: [ComboSpec; 1] = [ComboSpec {
    field: "activities.min_raid_difficulty",
    title: "Minimum raid difficulty",
    choices: &["LFR", "Normal", "Heroic", "Mythic"],
    get: |config| match config.activities.min_raid_difficulty {
        RaidDifficulty::Lfr => 0,
        RaidDifficulty::Normal => 1,
        RaidDifficulty::Heroic => 2,
        RaidDifficulty::Mythic => 3,
    },
    set: |config, index| {
        config.activities.min_raid_difficulty = match index {
            1 => RaidDifficulty::Normal,
            2 => RaidDifficulty::Heroic,
            3 => RaidDifficulty::Mythic,
            _ => RaidDifficulty::Lfr,
        }
    },
}];

pub static ACTIVITY_SPINS: [SpinSpec; 4] = [
    SpinSpec {
        field: "activities.min_raid_duration_seconds",
        title: "Minimum raid duration",
        subtitle: "Seconds; shorter raid pulls are discarded (0–10000)",
        min: 0.0,
        max: 10_000.0,
        step: 5.0,
        get: |config| f64::from(config.activities.min_raid_duration_seconds),
        set: |config, value| config.activities.min_raid_duration_seconds = value as i32,
    },
    SpinSpec {
        field: "activities.raid_overrun_seconds",
        title: "Raid overrun",
        subtitle: "Seconds recorded after a raid ends (0–60)",
        min: 0.0,
        max: 60.0,
        step: 1.0,
        get: |config| f64::from(config.activities.raid_overrun_seconds),
        set: |config, value| config.activities.raid_overrun_seconds = value as u32,
    },
    SpinSpec {
        field: "activities.min_keystone_level",
        title: "Minimum keystone level",
        subtitle: "Lower keys are not recorded (minimum 2)",
        min: 2.0,
        max: 100.0,
        step: 1.0,
        get: |config| f64::from(config.activities.min_keystone_level),
        set: |config, value| config.activities.min_keystone_level = value as u32,
    },
    SpinSpec {
        field: "activities.dungeon_overrun_seconds",
        title: "Dungeon overrun",
        subtitle: "Seconds recorded after a dungeon ends (0–60)",
        min: 0.0,
        max: 60.0,
        step: 1.0,
        get: |config| f64::from(config.activities.dungeon_overrun_seconds),
        set: |config, value| config.activities.dungeon_overrun_seconds = value as u32,
    },
];

pub static ACTIVITY_EXTRA_SWITCHES: [SwitchSpec; 4] = [
    SwitchSpec {
        field: "activities.current_raid_only",
        title: "Current raid tier only",
        subtitle: "Skip encounters from older raids",
        get: |config| config.activities.current_raid_only,
        set: |config, value| config.activities.current_raid_only = value,
    },
    SwitchSpec {
        field: "validate_log_paths",
        title: "Validate log folders",
        subtitle: "Require each chosen folder to be a World of Warcraft Logs directory",
        get: |config| config.validate_log_paths,
        set: |config, value| config.validate_log_paths = value,
    },
    SwitchSpec {
        field: "manual.enabled",
        title: "Manual recording",
        subtitle: "Show Start/Stop recording in the Manual category",
        get: |config| config.manual.enabled,
        set: |config, value| config.manual.enabled = value,
    },
    SwitchSpec {
        field: "manual.sound",
        title: "Manual recording sound",
        subtitle: "Play an alert when a manual recording starts or stops",
        get: |config| config.manual.sound,
        set: |config, value| config.manual.sound = value,
    },
];

pub static STORAGE_SPINS: [SpinSpec; 1] = [SpinSpec {
    field: "storage.limit",
    title: "Storage limit",
    subtitle: "GiB; 0 = Unlimited. Oldest unprotected recordings are deleted over the limit.",
    min: 0.0,
    max: 64_000.0,
    step: 5.0,
    get: |config| match config.storage.limit {
        StorageLimit::Unlimited => 0.0,
        StorageLimit::Gib(gib) => gib.get() as f64,
    },
    set: |config, value| {
        config.storage.limit = match std::num::NonZeroU64::new(value as u64) {
            None => StorageLimit::Unlimited,
            Some(gib) => StorageLimit::Gib(gib),
        }
    },
}];

pub static INTERFACE_SWITCHES: [SwitchSpec; 5] = [
    SwitchSpec {
        field: "storage.separate_buffer_dir",
        title: "Separate replay-buffer folder",
        subtitle: "Store the disk replay buffer outside the recording folder",
        get: |config| config.storage.separate_buffer_dir,
        set: |config, value| config.storage.separate_buffer_dir = value,
    },
    SwitchSpec {
        field: "interface.hide_empty_categories",
        title: "Hide empty categories",
        subtitle: "",
        get: |config| config.interface.hide_empty_categories,
        set: |config, value| config.interface.hide_empty_categories = value,
    },
    SwitchSpec {
        field: "interface.minimize_to_tray",
        title: "Minimize to tray",
        subtitle: "",
        get: |config| config.interface.minimize_to_tray,
        set: |config, value| config.interface.minimize_to_tray = value,
    },
    SwitchSpec {
        field: "interface.close_to_tray",
        title: "Close to tray",
        subtitle: "Keep recording in the background when the window closes",
        get: |config| config.interface.close_to_tray,
        set: |config, value| config.interface.close_to_tray = value,
    },
    SwitchSpec {
        field: "interface.start_minimized",
        title: "Start minimized",
        subtitle: "",
        get: |config| config.interface.start_minimized,
        set: |config, value| config.interface.start_minimized = value,
    },
];

pub static PATHS: [PathSpec; 7] = [
    PathSpec {
        field: "flavors.retail",
        title: "Retail",
        needs_write: false,
        get: |config| &config.flavors.retail.log_dir,
        set: |config, path| config.flavors.retail.log_dir = path,
        enabled: Some(EnabledAccess {
            get: |config| config.flavors.retail.enabled,
            set: |config, value| config.flavors.retail.enabled = value,
        }),
    },
    PathSpec {
        field: "flavors.retail_ptr",
        title: "Retail PTR",
        needs_write: false,
        get: |config| &config.flavors.retail_ptr.log_dir,
        set: |config, path| config.flavors.retail_ptr.log_dir = path,
        enabled: Some(EnabledAccess {
            get: |config| config.flavors.retail_ptr.enabled,
            set: |config, value| config.flavors.retail_ptr.enabled = value,
        }),
    },
    PathSpec {
        field: "flavors.classic",
        title: "Classic",
        needs_write: false,
        get: |config| &config.flavors.classic.log_dir,
        set: |config, path| config.flavors.classic.log_dir = path,
        enabled: Some(EnabledAccess {
            get: |config| config.flavors.classic.enabled,
            set: |config, value| config.flavors.classic.enabled = value,
        }),
    },
    PathSpec {
        field: "flavors.classic_ptr",
        title: "Classic PTR",
        needs_write: false,
        get: |config| &config.flavors.classic_ptr.log_dir,
        set: |config, path| config.flavors.classic_ptr.log_dir = path,
        enabled: Some(EnabledAccess {
            get: |config| config.flavors.classic_ptr.enabled,
            set: |config, value| config.flavors.classic_ptr.enabled = value,
        }),
    },
    PathSpec {
        field: "flavors.era",
        title: "Era",
        needs_write: false,
        get: |config| &config.flavors.era.log_dir,
        set: |config, path| config.flavors.era.log_dir = path,
        enabled: Some(EnabledAccess {
            get: |config| config.flavors.era.enabled,
            set: |config, value| config.flavors.era.enabled = value,
        }),
    },
    PathSpec {
        field: "storage.recording_dir",
        title: "Recording folder",
        needs_write: true,
        get: |config| &config.storage.recording_dir,
        set: |config, path| config.storage.recording_dir = path,
        enabled: None,
    },
    PathSpec {
        field: "storage.buffer_dir",
        title: "Replay-buffer folder",
        needs_write: true,
        get: |config| &config.storage.buffer_dir,
        set: |config, path| config.storage.buffer_dir = path,
        enabled: None,
    },
];

/// Dependency sensitivity: a disabled parent greys its children without
/// erasing their values.
pub fn row_sensitive(field: &str, config: &Config) -> bool {
    match field {
        "activities.min_raid_difficulty"
        | "activities.min_raid_duration_seconds"
        | "activities.current_raid_only"
        | "activities.raid_overrun_seconds" => config.activities.record_raids,
        "activities.min_keystone_level" | "activities.dungeon_overrun_seconds" => {
            config.activities.record_dungeons
        }
        "storage.buffer_dir" => config.storage.separate_buffer_dir,
        "manual.sound" => config.manual.enabled,
        "capture.audio_input" => config.capture.audio_input.is_some(),
        _ => true,
    }
}

// --- Apply pipeline ---

/// Opening of the feedback shown while `unsafe_reason` holds, so it can be
/// told apart and cleared once the recorder is idle again.
const UNSAFE_FEEDBACK: &str = "Settings cannot be applied";

/// Reconfiguration is unsafe while capturing, overrunning, or finalizing or
/// queueing media work.
pub fn unsafe_reason(snapshot: &AppSnapshot) -> Option<&'static str> {
    match snapshot.status {
        RecorderStatus::Recording { .. } => Some("while a recording is active"),
        RecorderStatus::Overrunning { .. } => Some("while a recording finishes its overrun"),
        RecorderStatus::Finalizing { .. } => Some("while a recording is being saved"),
        _ if snapshot.work.is_some() || snapshot.queued_jobs > 0 => {
            Some("while media work is in progress")
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum ApplyOutcome {
    Invalid(Vec<ValidationProblem>),
    Save(Box<Config>),
}

pub fn apply_outcome(draft: &Config) -> ApplyOutcome {
    let draft = draft.clone();
    let problems = draft.validate();
    if problems.is_empty() {
        ApplyOutcome::Save(Box::new(draft))
    } else {
        ApplyOutcome::Invalid(problems)
    }
}

/// Chooser probe matched to the field: log folders must be readable,
/// recording/buffer folders writable. Runs on GIO's blocking pool.
pub fn probe_folder(path: &Path, needs_write: bool) -> Result<(), String> {
    std::fs::read_dir(path).map_err(|error| format!("The folder cannot be read: {error}"))?;
    if needs_write {
        let probe = path.join(format!(".warcraft-recorder-probe-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|error| format!("The folder cannot be written: {error}"))?;
        let write_error = file
            .write_all(b"probe")
            .err()
            .map(|error| format!("The folder cannot be written: {error}"));
        drop(file);
        match (write_error, std::fs::remove_file(&probe).err()) {
            (Some(error), _) => return Err(error),
            (None, Some(error)) => return Err(format!("The folder cannot be written: {error}")),
            (None, None) => {}
        }
    }
    Ok(())
}

/// Subtitle text and whether the path needs Flatpak reauthorization.
pub fn path_state(path: &AuthorizedPath) -> (String, bool) {
    if path.path.as_os_str().is_empty() {
        return ("Not selected".to_owned(), false);
    }
    let shown = path.path.display();
    if path.is_authorized() {
        (shown.to_string(), false)
    } else {
        (
            format!("Permission required. Flatpak needs you to select this folder again: {shown}"),
            true,
        )
    }
}

/// The unavailable-device rule: a selected device missing from the list stays
/// visible as unavailable and remains selected until the user chooses.
pub fn audio_model(devices: &[AudioDevice], selected: &str) -> (Vec<AudioDevice>, u32) {
    let mut devices = devices.to_vec();
    let index = match devices.iter().position(|device| device.id == selected) {
        Some(index) => index,
        None => {
            devices.push(AudioDevice {
                id: selected.to_owned(),
                label: format!("{selected} - Unavailable"),
            });
            devices.len() - 1
        }
    };
    (devices, index as u32)
}

pub fn storage_summary(used_bytes: u64) -> String {
    format!(
        "Currently using {:.1} GiB.",
        used_bytes as f64 / (1024f64 * 1024.0 * 1024.0)
    )
}

// --- Widgets ---

type Registry = Rc<RefCell<Vec<(&'static str, gtk4::Widget)>>>;

pub struct Settings {
    pub dialog: adw::Dialog,
    sink: ActionSink,
    draft: Rc<RefCell<Config>>,
    /// The last config known applied; the discard warning compares the draft
    /// against it.
    baseline: Rc<RefCell<Config>>,
    /// Draft sent with `SaveConfig` and the wall-clock send time, until the
    /// snapshot confirms disk and runtime match.
    pending: Rc<RefCell<Option<(Config, i64)>>>,
    registry: Registry,
    /// Recorder/path controls disabled while reconfiguration is unsafe.
    gated: Vec<gtk4::Widget>,
    apply: gtk4::Button,
    feedback: gtk4::Label,
    target_row: adw::ActionRow,
    capture_warning: gtk4::Label,
    output_combo: adw::ComboRow,
    output_ids: Rc<RefCell<Vec<String>>>,
    input_combo: adw::ComboRow,
    input_ids: Rc<RefCell<Vec<String>>>,
    audio_group: adw::PreferencesGroup,
    advanced_box: gtk4::Box,
    /// What `advanced_box` was last built from; see `StatusCard`.
    rendered_warnings: RefCell<Vec<String>>,
    storage_group: adw::PreferencesGroup,
    protected_note: gtk4::Label,
    tray_note: gtk4::Label,
}

impl Settings {
    pub fn open(
        parent: &gtk4::Window,
        sink: ActionSink,
        snapshot: &AppSnapshot,
        tray_available: bool,
    ) -> Rc<Self> {
        let draft = Rc::new(RefCell::new(snapshot.config.clone()));
        let registry: Registry = Rc::new(RefCell::new(Vec::new()));
        // Closures owned by the dialog's widgets hold `Settings`, the
        // registry, the dialog, or their own row weakly: a strong reference
        // would cycle through the widget tree and leak the whole dialog.
        let refresh: Rc<dyn Fn()> = {
            let registry = Rc::downgrade(&registry);
            let draft = Rc::clone(&draft);
            Rc::new(move || {
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                let draft = draft.borrow();
                for (field, widget) in registry.borrow().iter() {
                    widget.set_sensitive(row_sensitive(field, &draft));
                    if path_needs_attention(field, &draft) {
                        widget.add_css_class("wr-needs-attention");
                    } else {
                        widget.remove_css_class("wr-needs-attention");
                    }
                }
            })
        };

        let stack = adw::ViewStack::new();
        let mut gated: Vec<gtk4::Widget> = Vec::new();

        // --- Capture -------------------------------------------------------
        let capture_page = adw::PreferencesPage::new();
        let capture_group = adw::PreferencesGroup::new();
        capture_group.set_title("Capture");
        capture_group.add(&combo_row(&CAPTURE_COMBOS[0], &draft, &registry, &refresh));
        for spec in &CAPTURE_SPINS {
            capture_group.add(&spin_row(spec, &draft, &registry, &refresh));
        }
        for spec in &CAPTURE_SWITCHES {
            capture_group.add(&switch_row(spec, &draft, &registry, &refresh));
        }
        capture_group.add(&combo_row(&CAPTURE_COMBOS[1], &draft, &registry, &refresh));
        capture_page.add(&capture_group);

        let target_group = adw::PreferencesGroup::new();
        target_group.set_title("Capture target");
        let target_row = adw::ActionRow::new();
        target_row.set_title("Screen or window");
        let reselect = gtk4::Button::with_label("Reselect capture target");
        reselect.set_valign(gtk4::Align::Center);
        reselect.set_tooltip_text(Some(
            "Ask the desktop portal for a new screen or window to record",
        ));
        {
            let sink = Rc::clone(&sink);
            let parent = parent.clone();
            reselect.connect_clicked(move |_| {
                present_reselect_dialog(parent.upcast_ref(), Rc::clone(&sink));
            });
        }
        gated.push(reselect.clone().upcast());
        // wr-capture follows the Wow.exe window by itself.
        reselect.set_visible(cfg!(unix));
        target_row.add_suffix(&reselect);
        target_group.add(&target_row);
        let capture_warning = gtk4::Label::new(None);
        capture_warning.set_wrap(true);
        capture_warning.set_xalign(0.0);
        capture_warning.add_css_class("warning");
        capture_warning.set_visible(false);
        target_group.add(&capture_warning);
        capture_page.add(&target_group);

        // --- Audio ---------------------------------------------------------
        let audio_page = adw::PreferencesPage::new();
        let audio_group = adw::PreferencesGroup::new();
        audio_group.set_title("Audio devices");
        audio_group.set_description(Some("Loading audio devices…"));
        let refresh_audio = gtk4::Button::from_icon_name("view-refresh-symbolic");
        refresh_audio.set_tooltip_text(Some("Refresh audio devices"));
        refresh_audio
            .update_property(&[gtk4::accessible::Property::Label("Refresh audio devices")]);
        refresh_audio.set_valign(gtk4::Align::Center);
        audio_group.set_header_suffix(Some(&refresh_audio));

        let output_combo = adw::ComboRow::new();
        output_combo.set_title("Output audio");
        output_combo.set_subtitle("Recorded game/system audio");
        let output_ids: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        connect_audio_combo(&output_combo, &output_ids, {
            let draft = Rc::clone(&draft);
            move |id| draft.borrow_mut().capture.audio_output = id
        });
        audio_group.add(&output_combo);

        let input_switch = adw::SwitchRow::new();
        input_switch.set_title("Record an input device");
        input_switch.set_active(snapshot.config.capture.audio_input.is_some());
        audio_group.add(&input_switch);
        let input_combo = adw::ComboRow::new();
        input_combo.set_title("Input audio");
        let input_ids: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        connect_audio_combo(&input_combo, &input_ids, {
            let draft = Rc::clone(&draft);
            move |id| {
                let mut draft = draft.borrow_mut();
                if draft.capture.audio_input.is_some() {
                    draft.capture.audio_input = Some(id);
                }
            }
        });
        registry
            .borrow_mut()
            .push(("capture.audio_input", input_combo.clone().upcast()));
        audio_group.add(&input_combo);
        audio_page.add(&audio_group);

        // --- Activities ----------------------------------------------------
        let activities_page = adw::PreferencesPage::new();
        let logs_group = adw::PreferencesGroup::new();
        logs_group.set_title("Combat logs");
        logs_group.set_description(Some(
            "Enable each World of Warcraft flavour and choose its Logs folder.",
        ));
        for spec in PATHS.iter().take(5) {
            let (row, select) = path_row(spec, &draft, &registry, &refresh, parent);
            gated.push(select.upcast());
            logs_group.add(&row);
        }
        let advanced_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        logs_group.add(&advanced_box);
        activities_page.add(&logs_group);

        let auto_group = adw::PreferencesGroup::new();
        auto_group.set_title("Automatic recording");
        for spec in &ACTIVITY_SWITCHES {
            auto_group.add(&switch_row(spec, &draft, &registry, &refresh));
        }
        auto_group.add(&combo_row(&ACTIVITY_COMBOS[0], &draft, &registry, &refresh));
        for spec in &ACTIVITY_SPINS {
            auto_group.add(&spin_row(spec, &draft, &registry, &refresh));
        }
        auto_group.add(&switch_row(
            &ACTIVITY_EXTRA_SWITCHES[0],
            &draft,
            &registry,
            &refresh,
        ));
        auto_group.add(&switch_row(
            &ACTIVITY_EXTRA_SWITCHES[1],
            &draft,
            &registry,
            &refresh,
        ));
        activities_page.add(&auto_group);

        let manual_group = adw::PreferencesGroup::new();
        manual_group.set_title("Manual and test recording");
        manual_group.add(&switch_row(
            &ACTIVITY_EXTRA_SWITCHES[2],
            &draft,
            &registry,
            &refresh,
        ));
        manual_group.add(&switch_row(
            &ACTIVITY_EXTRA_SWITCHES[3],
            &draft,
            &registry,
            &refresh,
        ));
        let test_row = adw::ButtonRow::new();
        test_row.set_title("Test recording…");
        {
            let sink = Rc::clone(&sink);
            test_row.connect_activated(move |_| {
                sink(ShellAction::TestRecording);
            });
        }
        gated.push(test_row.clone().upcast());
        manual_group.add(&test_row);
        activities_page.add(&manual_group);

        // --- Storage & interface ------------------------------------------
        let storage_page = adw::PreferencesPage::new();
        let storage_group = adw::PreferencesGroup::new();
        storage_group.set_title("Storage");
        for (index, spec) in PATHS.iter().enumerate().skip(5) {
            let (row, select) = path_row(spec, &draft, &registry, &refresh, parent);
            gated.push(select.upcast());
            storage_group.add(&row);
            if index == 5 {
                storage_group.add(&switch_row(
                    &INTERFACE_SWITCHES[0],
                    &draft,
                    &registry,
                    &refresh,
                ));
            }
        }
        for spec in &STORAGE_SPINS {
            storage_group.add(&spin_row(spec, &draft, &registry, &refresh));
        }
        let protected_note = gtk4::Label::new(Some(
            "Protected recordings exceed the storage limit; nothing more will be evicted.",
        ));
        protected_note.set_wrap(true);
        protected_note.set_xalign(0.0);
        protected_note.add_css_class("warning");
        protected_note.set_visible(false);
        storage_group.add(&protected_note);
        storage_page.add(&storage_group);

        let interface_group = adw::PreferencesGroup::new();
        interface_group.set_title("Interface");
        for spec in INTERFACE_SWITCHES.iter().skip(1) {
            interface_group.add(&switch_row(spec, &draft, &registry, &refresh));
        }
        let tray_note = gtk4::Label::new(Some(
            "No system tray was found, so closing the window quits Warcraft Recorder.",
        ));
        tray_note.set_wrap(true);
        tray_note.set_xalign(0.0);
        tray_note.add_css_class("dim-label");
        tray_note.set_visible(!tray_available);
        interface_group.add(&tray_note);
        storage_page.add(&interface_group);

        // --- Dialog scaffolding -------------------------------------------
        for (page, name, title, icon) in [
            (
                &capture_page,
                "capture",
                "Capture",
                "video-display-symbolic",
            ),
            (&audio_page, "audio", "Audio", "audio-speakers-symbolic"),
            (
                &activities_page,
                "activities",
                "Activities",
                "input-gaming-symbolic",
            ),
            (
                &storage_page,
                "storage",
                "Storage & interface",
                "drive-harddisk-symbolic",
            ),
        ] {
            let stack_page = stack.add_titled(page, Some(name), title);
            stack_page.set_icon_name(Some(icon));
        }

        let switcher = adw::ViewSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_policy(adw::ViewSwitcherPolicy::Wide);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&switcher));
        header.set_show_start_title_buttons(false);
        // The X closes the dialog; a discard warning guards unapplied edits.
        header.set_show_end_title_buttons(true);
        let cancel = gtk4::Button::with_label("Cancel");
        let apply = gtk4::Button::with_label("Apply");
        apply.add_css_class("suggested-action");
        let action_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        action_bar.set_halign(gtk4::Align::Center);
        action_bar.set_margin_top(8);
        action_bar.set_margin_bottom(8);
        action_bar.append(&apply);
        action_bar.append(&cancel);

        let feedback = gtk4::Label::new(None);
        feedback.set_wrap(true);
        feedback.set_xalign(0.0);
        feedback.set_margin_start(12);
        feedback.set_margin_end(12);
        feedback.set_margin_top(6);
        feedback.set_visible(false);

        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        body.append(&feedback);
        stack.set_vexpand(true);
        body.append(&stack);
        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.add_bottom_bar(&action_bar);
        toolbar_view.set_content(Some(&body));

        let dialog = adw::Dialog::new();
        dialog.set_title("Settings");
        dialog.set_content_width(760);
        dialog.set_content_height(680);
        dialog.set_child(Some(&toolbar_view));

        // Closing (X, Cancel, or Escape) with unapplied edits warns first.
        let baseline = Rc::new(RefCell::new(snapshot.config.clone()));
        dialog.set_can_close(false);
        {
            let draft = Rc::clone(&draft);
            let baseline = Rc::clone(&baseline);
            dialog.connect_close_attempt(move |dialog| {
                if *draft.borrow() == *baseline.borrow() {
                    dialog.force_close();
                    return;
                }
                let warning = adw::AlertDialog::new(
                    Some("Discard unapplied settings?"),
                    Some("Changes you made have not been applied and will be lost."),
                );
                warning.add_responses(&[("keep", "Keep editing"), ("discard", "Discard")]);
                warning.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
                warning.set_default_response(Some("keep"));
                warning.set_close_response("keep");
                let closing = dialog.clone();
                warning.connect_response(Some("discard"), move |_, _| closing.force_close());
                warning.present(Some(dialog));
            });
        }

        let settings = Rc::new(Self {
            dialog: dialog.clone(),
            sink,
            draft: Rc::clone(&draft),
            baseline,
            pending: Rc::new(RefCell::new(None)),
            registry,
            gated,
            apply: apply.clone(),
            feedback,
            target_row,
            capture_warning,
            output_combo,
            output_ids,
            input_combo,
            input_ids,
            audio_group,
            advanced_box,
            rendered_warnings: RefCell::new(Vec::new()),
            storage_group,
            protected_note,
            tray_note,
        });

        {
            let dialog = dialog.downgrade();
            cancel.connect_clicked(move |_| {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
            });
        }
        {
            let settings = Rc::downgrade(&settings);
            apply.connect_clicked(move |_| {
                if let Some(settings) = settings.upgrade() {
                    settings.on_apply();
                }
            });
        }
        {
            let settings = Rc::downgrade(&settings);
            let refresh = Rc::clone(&refresh);
            input_switch.connect_active_notify(move |switch| {
                let Some(settings) = settings.upgrade() else {
                    return;
                };
                let mut draft = settings.draft.borrow_mut();
                if switch.is_active() {
                    if draft.capture.audio_input.is_none() {
                        let ids = settings.input_ids.borrow();
                        let selected = settings.input_combo.selected() as usize;
                        draft.capture.audio_input = Some(
                            ids.get(selected)
                                .cloned()
                                .unwrap_or_else(|| "default_input".to_owned()),
                        );
                    }
                } else {
                    draft.capture.audio_input = None;
                }
                drop(draft);
                refresh();
            });
        }
        {
            let settings = Rc::downgrade(&settings);
            refresh_audio.connect_clicked(move |_| {
                if let Some(settings) = settings.upgrade() {
                    settings.load_audio();
                }
            });
        }

        refresh();
        settings.apply_snapshot(snapshot);
        settings.load_audio();
        dialog.present(Some(parent));
        settings
    }

    fn on_apply(&self) {
        self.clear_marks();
        match apply_outcome(&self.draft.borrow()) {
            ApplyOutcome::Invalid(problems) => self.show_problems(&problems),
            ApplyOutcome::Save(draft) => {
                if (self.sink)(ShellAction::Command(Command::SaveConfig {
                    draft: draft.clone(),
                })) {
                    *self.pending.borrow_mut() = Some((*draft, now_unix_ms()));
                    self.set_feedback("Applying…", "dim-label");
                    self.apply.set_sensitive(false);
                } else {
                    self.set_feedback("The app is busy, try again in a moment.", "warning");
                }
            }
        }
    }

    fn show_problems(&self, problems: &[ValidationProblem]) {
        let text = problems
            .iter()
            .map(|problem| format!("• {}", problem.message))
            .collect::<Vec<_>>()
            .join("\n");
        self.set_feedback(&text, "error");
        let registry = self.registry.borrow();
        for problem in problems {
            if let Some((_, widget)) = registry.iter().find(|(field, _)| *field == problem.field) {
                widget.add_css_class("error");
                widget.set_tooltip_text(Some(&problem.message));
            }
        }
    }

    fn clear_marks(&self) {
        for (_, widget) in self.registry.borrow().iter() {
            widget.remove_css_class("error");
        }
    }

    fn set_feedback(&self, text: &str, css: &str) {
        for class in ["error", "warning", "success", "dim-label"] {
            self.feedback.remove_css_class(class);
        }
        self.feedback.add_css_class(css);
        self.feedback.set_label(text);
        self.feedback.set_visible(!text.is_empty());
    }

    /// Snapshot-driven state: unsafe gating, capture-target status, inline
    /// recorder warning, storage usage, advanced-logging warnings, and the
    /// Apply confirmation.
    pub fn apply_snapshot(&self, snapshot: &AppSnapshot) {
        let reason = unsafe_reason(snapshot);
        let pending = self.pending.borrow().clone();
        self.apply
            .set_sensitive(reason.is_none() && pending.is_none());
        for widget in &self.gated {
            widget.set_sensitive(reason.is_none());
        }
        if let Some(reason) = reason {
            self.set_feedback(
                &format!("{UNSAFE_FEEDBACK} {reason}. The fields stay visible; Apply is re-enabled when the recorder is idle."),
                "warning",
            );
        } else if self.feedback.label().starts_with(UNSAFE_FEEDBACK) {
            self.set_feedback("", "dim-label");
        }

        self.target_row.set_subtitle(if cfg!(windows) {
            "Follows the World of Warcraft window automatically"
        } else if snapshot.config.capture.capture_target_token.is_some() {
            "Selected: restored automatically from the saved portal token"
        } else {
            "Not selected yet: the desktop portal will ask on the next capture"
        });

        // The recorder's own capture-settings rejection, shown inline instead
        // of a duplicated compatibility table.
        let capture_problem = snapshot
            .problems
            .iter()
            .rev()
            .find(|problem| problem.summary == "The capture settings are not usable.");
        match capture_problem {
            Some(problem) => {
                let detail = problem.safe_detail.as_deref().unwrap_or("");
                self.capture_warning
                    .set_label(&format!("{} {detail}", problem.summary));
                self.capture_warning.set_visible(true);
            }
            None => self.capture_warning.set_visible(false),
        }

        self.storage_group
            .set_description(Some(&storage_summary(snapshot.storage_used_bytes)));
        self.protected_note
            .set_visible(snapshot.protected_over_limit);

        let warnings = super::status::advanced_logging_warnings(snapshot);
        if *self.rendered_warnings.borrow() != warnings {
            super::clear_box(&self.advanced_box);
            for warning in &warnings {
                let label = gtk4::Label::new(Some(warning));
                label.set_wrap(true);
                label.set_xalign(0.0);
                label.add_css_class("warning");
                label.add_css_class("caption");
                self.advanced_box.append(&label);
            }
            *self.rendered_warnings.borrow_mut() = warnings;
        }

        if let Some((sent, sent_at_ms)) = pending
            && snapshot.config == sent
        {
            *self.pending.borrow_mut() = None;
            *self.baseline.borrow_mut() = sent;
            let runtime_problems: Vec<String> = snapshot
                .problems
                .iter()
                .filter(|problem| problem.occurred_unix_ms >= sent_at_ms)
                .map(|problem| format!("• {}", problem.summary))
                .collect();
            if runtime_problems.is_empty() && snapshot.setup_problems.is_empty() {
                self.set_feedback("Saved.", "success");
            } else if snapshot.setup_problems.is_empty() {
                self.set_feedback(
                    &format!("Saved, but with problems:\n{}", runtime_problems.join("\n")),
                    "warning",
                );
            }
        }
        if self.pending.borrow().is_some() {
            // Coordinator rejected the save without changing its config.
            if !snapshot.setup_problems.is_empty() {
                *self.pending.borrow_mut() = None;
                self.show_problems(&snapshot.setup_problems);
            }
        } else if unsafe_reason(snapshot).is_none() {
            self.apply.set_sensitive(true);
        }
    }

    pub fn set_tray_available(&self, available: bool) {
        self.tray_note.set_visible(!available);
    }

    /// Enumerate GSR audio devices off the GTK thread and fill both combos,
    /// keeping an unavailable selected device visible and selected.
    fn load_audio(&self) {
        let draft = Rc::clone(&self.draft);
        let output_combo = self.output_combo.clone();
        let output_ids = Rc::clone(&self.output_ids);
        let input_combo = self.input_combo.clone();
        let input_ids = Rc::clone(&self.input_ids);
        let group = self.audio_group.clone();
        group.set_description(Some("Loading audio devices…"));
        gtk4::glib::spawn_future_local(async move {
            let result = gtk4::gio::spawn_blocking(|| Recorder::new().audio_devices())
                .await
                .unwrap_or_else(|_| {
                    Err(warcraft_recorder::recorder::RecorderError::SpawnFailed {
                        message: "audio discovery crashed".to_owned(),
                        log_tail: String::new(),
                    })
                });
            let devices = match result {
                Ok(devices) => {
                    group.set_description(Some(
                        "Devices reported by gpu-screen-recorder. A selected device that is \
                         unavailable stays selected until you choose another.",
                    ));
                    devices
                }
                Err(error) => {
                    group.set_description(Some(&format!(
                        "Audio devices could not be listed ({error:?}). The saved selection is kept."
                    )));
                    warcraft_recorder::recorder::AudioDevices::default()
                }
            };
            let (selected_output, selected_input) = {
                let draft = draft.borrow();
                (
                    draft.capture.audio_output.clone(),
                    draft
                        .capture
                        .audio_input
                        .clone()
                        .unwrap_or_else(|| "default_input".to_owned()),
                )
            };
            fill_combo(
                &output_combo,
                &output_ids,
                &devices.outputs,
                &selected_output,
            );
            fill_combo(&input_combo, &input_ids, &devices.inputs, &selected_input);
        });
    }
}

fn fill_combo(
    combo: &adw::ComboRow,
    ids: &Rc<RefCell<Vec<String>>>,
    devices: &[AudioDevice],
    selected: &str,
) {
    let (model, index) = audio_model(devices, selected);
    *ids.borrow_mut() = model.iter().map(|device| device.id.clone()).collect();
    let labels: Vec<&str> = model.iter().map(|device| device.label.as_str()).collect();
    combo.set_model(Some(&gtk4::StringList::new(&labels)));
    combo.set_selected(index);
}

/// Connected once per combo; `fill_combo` refills it on every device refresh.
fn connect_audio_combo(
    combo: &adw::ComboRow,
    ids: &Rc<RefCell<Vec<String>>>,
    on_change: impl Fn(String) + 'static,
) {
    let ids = Rc::clone(ids);
    combo.connect_selected_notify(move |combo| {
        if let Some(id) = ids.borrow().get(combo.selected() as usize) {
            on_change(id.clone());
        }
    });
}

fn spin_row(
    spec: &'static SpinSpec,
    draft: &Rc<RefCell<Config>>,
    registry: &Registry,
    refresh: &Rc<dyn Fn()>,
) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(spec.min, spec.max, spec.step);
    row.set_title(spec.title);
    if !spec.subtitle.is_empty() {
        row.set_subtitle(spec.subtitle);
    }
    row.set_digits(0);
    row.set_value((spec.get)(&draft.borrow()));
    let draft = Rc::clone(draft);
    let refresh = Rc::clone(refresh);
    row.connect_value_notify(move |row| {
        (spec.set)(&mut draft.borrow_mut(), row.value());
        refresh();
    });
    registry
        .borrow_mut()
        .push((spec.field, row.clone().upcast()));
    row
}

fn switch_row(
    spec: &'static SwitchSpec,
    draft: &Rc<RefCell<Config>>,
    registry: &Registry,
    refresh: &Rc<dyn Fn()>,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::new();
    row.set_title(spec.title);
    if !spec.subtitle.is_empty() {
        row.set_subtitle(spec.subtitle);
    }
    row.set_active((spec.get)(&draft.borrow()));
    let draft = Rc::clone(draft);
    let refresh = Rc::clone(refresh);
    row.connect_active_notify(move |row| {
        (spec.set)(&mut draft.borrow_mut(), row.is_active());
        refresh();
    });
    registry
        .borrow_mut()
        .push((spec.field, row.clone().upcast()));
    row
}

fn combo_row(
    spec: &'static ComboSpec,
    draft: &Rc<RefCell<Config>>,
    registry: &Registry,
    refresh: &Rc<dyn Fn()>,
) -> adw::ComboRow {
    let row = adw::ComboRow::new();
    row.set_title(spec.title);
    row.set_model(Some(&gtk4::StringList::new(spec.choices)));
    row.set_selected((spec.get)(&draft.borrow()));
    let draft = Rc::clone(draft);
    let refresh = Rc::clone(refresh);
    row.connect_selected_notify(move |row| {
        (spec.set)(&mut draft.borrow_mut(), row.selected());
        refresh();
    });
    registry
        .borrow_mut()
        .push((spec.field, row.clone().upcast()));
    row
}

/// A required folder that is unset, or one whose authorization did not
/// survive the move into the sandbox, is the only thing standing between a
/// fresh install and a working recorder. Rows the user
/// does not have to fill in stay quiet: a greyed-out dependant such as the
/// replay-buffer folder while it shares the recording folder, and the log
/// folder of a flavour that is switched off.
fn path_needs_attention(field: &str, config: &Config) -> bool {
    let Some(spec) = PATHS.iter().find(|spec| spec.field == field) else {
        return false;
    };
    if !row_sensitive(field, config) {
        return false;
    }
    if let Some(enabled) = &spec.enabled
        && !(enabled.get)(config)
    {
        return false;
    }
    let path = (spec.get)(config);
    path.path.as_os_str().is_empty() || path_state(path).1
}

/// A directory row: optional flavour switch, path/authorization subtitle, and
/// the native folder chooser. Selection probes access on GIO's blocking pool
/// and stores an authorized path only when the probe passes; cancellation
/// changes nothing.
fn path_row(
    spec: &'static PathSpec,
    draft: &Rc<RefCell<Config>>,
    registry: &Registry,
    refresh: &Rc<dyn Fn()>,
    parent: &gtk4::Window,
) -> (adw::ActionRow, gtk4::Button) {
    let row = adw::ActionRow::new();
    row.set_title(spec.title);
    let (subtitle, needs_reauth) = path_state((spec.get)(&draft.borrow()));
    row.set_subtitle(&subtitle);

    if let Some(EnabledAccess {
        get: get_enabled,
        set: set_enabled,
    }) = spec.enabled
    {
        let switch = gtk4::Switch::new();
        switch.set_valign(gtk4::Align::Center);
        switch.set_active(get_enabled(&draft.borrow()));
        let draft_for_switch = Rc::clone(draft);
        let refresh = Rc::clone(refresh);
        switch.connect_active_notify(move |switch| {
            set_enabled(&mut draft_for_switch.borrow_mut(), switch.is_active());
            refresh();
        });
        row.add_prefix(&switch);
    }

    let select = gtk4::Button::with_label(if needs_reauth {
        "Select this folder…"
    } else {
        "Select folder…"
    });
    select.set_valign(gtk4::Align::Center);
    {
        let draft = Rc::clone(draft);
        let weak_row = row.downgrade();
        let parent = parent.clone();
        let refresh = Rc::clone(refresh);
        select.connect_clicked(move |button| {
            let Some(row) = weak_row.upgrade() else {
                return;
            };
            let chooser = gtk4::FileDialog::new();
            chooser.set_title(&format!("Choose the {} folder", spec.title));
            let current = (spec.get)(&draft.borrow()).path.clone();
            if !current.as_os_str().is_empty() {
                chooser.set_initial_folder(Some(&gtk4::gio::File::for_path(&current)));
            }
            let draft = Rc::clone(&draft);
            let button = button.clone();
            let refresh = Rc::clone(&refresh);
            chooser.select_folder(
                Some(&parent),
                None::<&gtk4::gio::Cancellable>,
                move |result| {
                    let Ok(file) = result else {
                        return; // Cancelled: nothing changes.
                    };
                    let Some(path) = file.path() else {
                        row.set_subtitle("The chosen location has no filesystem path.");
                        return;
                    };
                    let probe_path: PathBuf = path.clone();
                    let draft = Rc::clone(&draft);
                    let row = row.clone();
                    let button = button.clone();
                    let refresh = Rc::clone(&refresh);
                    gtk4::glib::spawn_future_local(async move {
                        let probed = gtk4::gio::spawn_blocking(move || {
                            probe_folder(&probe_path, spec.needs_write)
                        })
                        .await
                        .unwrap_or_else(|_| Err("the access check crashed".to_owned()));
                        match probed {
                            Ok(()) => {
                                (spec.set)(
                                    &mut draft.borrow_mut(),
                                    AuthorizedPath::authorized(&path),
                                );
                                row.remove_css_class("error");
                                let (subtitle, _) = path_state((spec.get)(&draft.borrow()));
                                row.set_subtitle(&subtitle);
                                button.set_label("Select folder…");
                                refresh();
                            }
                            Err(message) => {
                                row.add_css_class("error");
                                row.set_subtitle(&message);
                            }
                        }
                    });
                },
            );
        });
    }
    row.add_suffix(&select);
    registry
        .borrow_mut()
        .push((spec.field, row.clone().upcast()));
    (row, select)
}

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_recorder::config::{FlavorConfig, PathAuthorization};

    fn ready_config() -> Config {
        let mut config = Config::default();
        config.storage.recording_dir = AuthorizedPath::authorized("/recordings");
        config.flavors.retail = FlavorConfig {
            enabled: true,
            log_dir: AuthorizedPath::authorized("/wow/_retail_/Logs"),
        };
        config
    }

    fn snapshot(status: RecorderStatus) -> AppSnapshot {
        crate::ui::window::tests::snapshot_with(status, ready_config(), Vec::new())
    }

    #[test]
    fn only_folders_the_user_must_pick_ask_for_attention() {
        let mut config = ready_config();
        // Both required folders are set, and the buffer folder shares the
        // recording folder: nothing to ask for.
        assert!(!path_needs_attention("storage.recording_dir", &config));
        assert!(!path_needs_attention("flavors.retail", &config));
        assert!(!path_needs_attention("storage.buffer_dir", &config));

        // Turning the dependant on exposes it as unset, and only then.
        config.storage.separate_buffer_dir = true;
        assert!(path_needs_attention("storage.buffer_dir", &config));
        config.storage.buffer_dir = AuthorizedPath::authorized("/buffer");
        assert!(!path_needs_attention("storage.buffer_dir", &config));

        // Unset, and imported-but-unauthorized, both ask.
        config.storage.recording_dir = AuthorizedPath::unset();
        assert!(path_needs_attention("storage.recording_dir", &config));
        config.flavors.retail.log_dir = AuthorizedPath {
            path: PathBuf::from("/wow/_retail_/Logs"),
            authorization: PathAuthorization::ImportedInactive,
        };
        assert!(path_needs_attention("flavors.retail", &config));

        // A flavour that is switched off needs no log folder.
        config.flavors.retail.enabled = false;
        assert!(!path_needs_attention("flavors.retail", &config));
    }

    #[test]
    fn spin_defaults_stay_within_bounds() {
        let config = Config::default();
        // Spin bounds must agree with config validation, so a default always
        // renders inside them.
        for spec in CAPTURE_SPINS
            .iter()
            .chain(&ACTIVITY_SPINS)
            .chain(&STORAGE_SPINS)
        {
            let value = (spec.get)(&config);
            assert!(
                (spec.min..=spec.max).contains(&value),
                "{} default {value} outside {}..={}",
                spec.field,
                spec.min,
                spec.max
            );
        }
        // Zero storage limit means Unlimited, not a zero-byte quota.
        let mut unlimited = config.clone();
        (STORAGE_SPINS[0].set)(&mut unlimited, 0.0);
        assert_eq!(unlimited.storage.limit, StorageLimit::Unlimited);
        assert_eq!((STORAGE_SPINS[0].get)(&unlimited), 0.0);
    }

    #[test]
    fn unsafe_reason_covers_queued_media_work() {
        let mut queued = snapshot(RecorderStatus::Ready);
        assert_eq!(unsafe_reason(&queued), None);
        queued.queued_jobs = 1;
        assert!(unsafe_reason(&queued).is_some());
    }

    #[test]
    fn path_state_reports_reauthorization_and_audio_model_keeps_unavailable() {
        assert_eq!(
            path_state(&AuthorizedPath::unset()),
            ("Not selected".to_owned(), false)
        );
        let (subtitle, reauth) = path_state(&AuthorizedPath {
            path: PathBuf::from("/old/Logs"),
            authorization: PathAuthorization::ImportedInactive,
        });
        assert!(reauth);
        assert!(subtitle.contains("Permission required"));
        assert!(subtitle.contains("/old/Logs"));
        let (subtitle, reauth) = path_state(&AuthorizedPath::authorized("/new/Logs"));
        assert!(!reauth);
        assert_eq!(subtitle, "/new/Logs");

        let devices = [
            AudioDevice {
                id: "default_output".to_owned(),
                label: "Default output device".to_owned(),
            },
            AudioDevice {
                id: "device:alpha".to_owned(),
                label: "Speakers".to_owned(),
            },
        ];
        let (model, index) = audio_model(&devices, "device:alpha");
        assert_eq!(model.len(), 2);
        assert_eq!(index, 1);
        let (model, index) = audio_model(&devices, "device:gone");
        assert_eq!(model.len(), 3);
        assert_eq!(index, 2);
        assert_eq!(model[2].label, "device:gone - Unavailable");
    }
}
