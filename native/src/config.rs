// SPDX-License-Identifier: GPL-3.0-or-later

//! Native configuration persistence.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};

use crate::domain::{
    Category, Codec, DeathMarkerVisibility, MarkerVisibility, RaidDifficulty, StorageLimit,
};

const CONFIG_VERSION: u32 = 1;
pub const APP_ID: &str = "io.github.JohanWes.WarcraftRecorder";
pub const CONFIG_FILENAME: &str = "config.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathAuthorization {
    Unset,
    ImportedInactive,
    Authorized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedPath {
    pub path: PathBuf,
    pub authorization: PathAuthorization,
}

impl AuthorizedPath {
    pub fn unset() -> Self {
        Self {
            path: PathBuf::new(),
            authorization: PathAuthorization::Unset,
        }
    }

    pub fn authorized(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            authorization: PathAuthorization::Authorized,
        }
    }

    pub fn is_authorized(&self) -> bool {
        self.authorization == PathAuthorization::Authorized && !self.path.as_os_str().is_empty()
    }
}

impl Default for AuthorizedPath {
    fn default() -> Self {
        Self::unset()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlavorConfig {
    pub enabled: bool,
    pub log_dir: AuthorizedPath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FlavorSettings {
    pub retail: FlavorConfig,
    pub retail_ptr: FlavorConfig,
    pub classic: FlavorConfig,
    pub classic_ptr: FlavorConfig,
    pub era: FlavorConfig,
}

impl FlavorSettings {
    fn in_field_order(&self) -> [(&'static str, &FlavorConfig); 5] {
        [
            ("flavors.retail", &self.retail),
            ("flavors.retail_ptr", &self.retail_ptr),
            ("flavors.classic", &self.classic),
            ("flavors.classic_ptr", &self.classic_ptr),
            ("flavors.era", &self.era),
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySettings {
    pub record_raids: bool,
    pub record_dungeons: bool,
    pub record_two_v_two: bool,
    pub record_three_v_three: bool,
    pub record_five_v_five: bool,
    pub record_skirmish: bool,
    pub record_solo_shuffle: bool,
    pub record_battlegrounds: bool,
    pub record_challenge_modes: bool,
    pub min_keystone_level: u32,
    pub min_raid_difficulty: RaidDifficulty,
    pub min_raid_duration_seconds: i32,
    pub current_raid_only: bool,
    pub raid_overrun_seconds: u32,
    pub dungeon_overrun_seconds: u32,
}

impl Default for ActivitySettings {
    fn default() -> Self {
        Self {
            record_raids: true,
            record_dungeons: true,
            record_two_v_two: true,
            record_three_v_three: true,
            record_five_v_five: true,
            record_skirmish: true,
            record_solo_shuffle: true,
            record_battlegrounds: true,
            record_challenge_modes: true,
            min_keystone_level: 2,
            min_raid_difficulty: RaidDifficulty::Lfr,
            min_raid_duration_seconds: 15,
            current_raid_only: false,
            raid_overrun_seconds: 15,
            dungeon_overrun_seconds: 5,
        }
    }
}

impl ActivitySettings {
    fn any_enabled(&self) -> bool {
        self.record_raids
            || self.record_dungeons
            || self.record_two_v_two
            || self.record_three_v_three
            || self.record_five_v_five
            || self.record_skirmish
            || self.record_solo_shuffle
            || self.record_battlegrounds
            || self.record_challenge_modes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSettings {
    pub recording_dir: AuthorizedPath,
    pub limit: StorageLimit,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            recording_dir: AuthorizedPath::unset(),
            limit: StorageLimit::Gib(NonZeroU64::new(50).expect("50 is nonzero")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSettings {
    pub fps: u32,
    pub codec: Codec,
    pub bitrate_kbps: u32,
    pub extra_lead_in_seconds: u32,
    pub capture_cursor: bool,
    pub audio_output: String,
    pub audio_input: Option<String>,
    pub capture_target_token: Option<String>,
}

/// The combat log reveals activities within a fraction of a second, so the
/// replay buffer only has to hold the lead-in plus this margin, which also
/// covers a capture that starts while the previous one is still saving.
const REPLAY_MARGIN_SECONDS: u32 = 10;

impl CaptureSettings {
    pub fn replay_buffer_seconds(&self) -> u32 {
        self.extra_lead_in_seconds + REPLAY_MARGIN_SECONDS
    }
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            fps: 60,
            codec: Codec::H264,
            bitrate_kbps: 20_000,
            extra_lead_in_seconds: 0,
            capture_cursor: false,
            audio_output: "default_output".to_owned(),
            audio_input: None,
            capture_target_token: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSettings {
    pub enabled: bool,
    pub sound: bool,
}

impl Default for ManualSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            sound: true,
        }
    }
}

/// Sizes the user dragged: the player/library divider and the table columns.
/// Empty on a clean install, which is the only state in which the player pane
/// may autoscale itself to the video's aspect ratio.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSettings {
    pub player_split: Option<i32>,
    /// Column title to width. Titles are shared across category families, so a
    /// width dragged in Mythic+ also applies to the raid column of that name.
    pub column_widths: BTreeMap<String, i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceSettings {
    pub hide_empty_categories: bool,
    pub death_markers: DeathMarkerVisibility,
    pub encounter_markers: MarkerVisibility,
    pub round_markers: MarkerVisibility,
    pub selected_category: Category,
    pub minimize_to_tray: bool,
    pub close_to_tray: bool,
    pub start_minimized: bool,
    #[serde(default)]
    pub layout: LayoutSettings,
}

impl Default for InterfaceSettings {
    fn default() -> Self {
        Self {
            hide_empty_categories: false,
            death_markers: DeathMarkerVisibility::Own,
            encounter_markers: MarkerVisibility::Visible,
            round_markers: MarkerVisibility::Visible,
            selected_category: Category::ThreeVThree,
            // Windows minimizes to the taskbar, so Win+D can restore it.
            minimize_to_tray: !cfg!(windows),
            close_to_tray: true,
            start_minimized: false,
            layout: LayoutSettings::default(),
        }
    }
}

/// Keys missing from the file take their defaults, so a hand-edited or older
/// config is not rejected over one absent field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub flavors: FlavorSettings,
    pub activities: ActivitySettings,
    pub storage: StorageSettings,
    pub capture: CaptureSettings,
    pub manual: ManualSettings,
    pub interface: InterfaceSettings,
    pub validate_log_paths: bool,
    /// The version whose release notes were last acknowledged. Empty until
    /// the user closes the "What's new" dialog, so an install that predates
    /// the dialog still gets one for the version it just updated to.
    #[serde(default)]
    pub last_seen_version: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            flavors: FlavorSettings::default(),
            activities: ActivitySettings::default(),
            storage: StorageSettings::default(),
            capture: CaptureSettings::default(),
            manual: ManualSettings::default(),
            interface: InterfaceSettings::default(),
            validate_log_paths: true,
            // A clean install has no earlier version to report on.
            last_seen_version: crate::VERSION.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationProblem {
    pub field: &'static str,
    pub message: String,
}

impl ValidationProblem {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Vec<ValidationProblem> {
        let mut problems = self.persistence_problems();

        validate_active_path(
            &mut problems,
            "storage.recording_dir",
            "Choose a recording directory.",
            "Choose the recording directory again to authorize access.",
            &self.storage.recording_dir,
        );

        if !self
            .flavors
            .in_field_order()
            .iter()
            .any(|(_, flavor)| flavor.enabled)
        {
            problems.push(ValidationProblem::new(
                "flavors",
                "Enable at least one World of Warcraft flavor.",
            ));
        }

        for (field, flavor) in self.flavors.in_field_order() {
            validate_flavor(&mut problems, field, flavor, self.validate_log_paths);
        }

        if !self.activities.any_enabled() {
            problems.push(ValidationProblem::new(
                "activities",
                "Enable at least one automatic activity type.",
            ));
        }

        problems
    }

    fn persistence_problems(&self) -> Vec<ValidationProblem> {
        let mut problems = Vec::new();

        if self.version != CONFIG_VERSION {
            problems.push(ValidationProblem::new(
                "version",
                format!("Unsupported config version {}.", self.version),
            ));
        }

        validate_path_state(
            &mut problems,
            "storage.recording_dir",
            &self.storage.recording_dir,
        );
        for (field, flavor) in self.flavors.in_field_order() {
            validate_path_state(&mut problems, field, &flavor.log_dir);
        }
        if !(15..=60).contains(&self.capture.fps) {
            problems.push(ValidationProblem::new(
                "capture.fps",
                "FPS must be between 15 and 60.",
            ));
        }
        if !(1_000..=200_000).contains(&self.capture.bitrate_kbps) {
            problems.push(ValidationProblem::new(
                "capture.bitrate_kbps",
                "Bitrate must be between 1000 and 200000 Kbps.",
            ));
        }
        if self.capture.extra_lead_in_seconds > 30 {
            problems.push(ValidationProblem::new(
                "capture.extra_lead_in_seconds",
                "Lead-in must be between 0 and 30 seconds.",
            ));
        }
        if self.capture.audio_output.trim().is_empty() {
            problems.push(ValidationProblem::new(
                "capture.audio_output",
                "Choose an output-audio device.",
            ));
        }
        if self
            .capture
            .audio_input
            .as_ref()
            .is_some_and(|input| input.trim().is_empty())
        {
            problems.push(ValidationProblem::new(
                "capture.audio_input",
                "Disable input audio or choose an input-audio device.",
            ));
        }
        if self
            .capture
            .capture_target_token
            .as_ref()
            .is_some_and(String::is_empty)
        {
            problems.push(ValidationProblem::new(
                "capture.capture_target_token",
                "The capture-target token cannot be empty.",
            ));
        }
        if self.activities.min_raid_duration_seconds > 10_000 {
            problems.push(ValidationProblem::new(
                "activities.min_raid_duration_seconds",
                "Minimum raid duration cannot exceed 10000 seconds.",
            ));
        }
        if self.activities.raid_overrun_seconds > 60 {
            problems.push(ValidationProblem::new(
                "activities.raid_overrun_seconds",
                "Raid overrun must be between 0 and 60 seconds.",
            ));
        }
        if self.activities.dungeon_overrun_seconds > 60 {
            problems.push(ValidationProblem::new(
                "activities.dungeon_overrun_seconds",
                "Dungeon overrun must be between 0 and 60 seconds.",
            ));
        }

        problems
    }

    /// A file that is rejected is copied to `<name>.bad` first: the caller
    /// falls back to defaults, and its next save replaces the original.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = fs::read(path).map_err(|source| map_read_error(path, source))?;
        let result = match serde_json::from_slice::<Self>(&bytes) {
            Err(source) => Err(ConfigError::InvalidJson {
                path: path.to_owned(),
                source,
            }),
            Ok(config) => match config.persistence_problems() {
                problems if problems.is_empty() => Ok(config),
                problems => Err(ConfigError::Validation(problems)),
            },
        };
        if result.is_err() {
            let mut bad = path.as_os_str().to_owned();
            bad.push(".bad");
            let _ = fs::copy(path, bad);
        }
        result
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let problems = self.persistence_problems();
        if !problems.is_empty() {
            return Err(ConfigError::Validation(problems));
        }
        write_atomic(self, path)
    }
}

fn validate_path_state(
    problems: &mut Vec<ValidationProblem>,
    field: &'static str,
    path: &AuthorizedPath,
) {
    let empty = path.path.as_os_str().is_empty();
    let consistent = matches!(
        (empty, path.authorization),
        (true, PathAuthorization::Unset)
            | (
                false,
                PathAuthorization::ImportedInactive | PathAuthorization::Authorized
            )
    );
    if !consistent {
        problems.push(ValidationProblem::new(
            field,
            "The saved path and its authorization state are inconsistent.",
        ));
    }
    if path.authorization == PathAuthorization::Authorized && !path.path.is_absolute() {
        problems.push(ValidationProblem::new(
            field,
            "Choose an absolute directory path.",
        ));
    }
}

fn validate_active_path(
    problems: &mut Vec<ValidationProblem>,
    field: &'static str,
    missing_message: &'static str,
    unauthorized_message: &'static str,
    path: &AuthorizedPath,
) {
    if path.path.as_os_str().is_empty() {
        problems.push(ValidationProblem::new(field, missing_message));
    } else if !path.is_authorized() {
        problems.push(ValidationProblem::new(field, unauthorized_message));
    }
}

fn validate_flavor(
    problems: &mut Vec<ValidationProblem>,
    field: &'static str,
    flavor: &FlavorConfig,
    validate_log_paths: bool,
) {
    if !flavor.enabled {
        return;
    }
    if flavor.log_dir.path.as_os_str().is_empty() {
        problems.push(ValidationProblem::new(
            field,
            "Choose the enabled flavor's Logs directory.",
        ));
    } else if !flavor.log_dir.is_authorized() {
        problems.push(ValidationProblem::new(
            field,
            "Choose the enabled flavor's Logs directory again to authorize access.",
        ));
    } else if validate_log_paths && flavor.log_dir.path.file_name() != Some(OsStr::new("Logs")) {
        problems.push(ValidationProblem::new(
            field,
            "Choose this flavor's World of Warcraft Logs directory.",
        ));
    }
}

fn write_atomic(config: &Config, path: &Path) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or_else(|| ConfigError::Io {
        operation: "resolve config parent",
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        operation: "create config directory",
        path: parent.to_owned(),
        source,
    })?;

    let pretty =
        serde_json::to_string_pretty(config).map_err(|source| ConfigError::InvalidJson {
            path: path.to_owned(),
            source,
        })?;
    let filename = path.file_name().ok_or_else(|| ConfigError::Io {
        operation: "resolve config filename",
        path: path.to_owned(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "config path has no filename"),
    })?;
    let mut temporary_name = filename.to_os_string();
    temporary_name.push(".tmp");
    let temporary_path = path.with_file_name(temporary_name);
    let _ = fs::remove_file(&temporary_path);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary_path)
            .map_err(|source| ConfigError::Io {
                operation: "create temporary config",
                path: temporary_path.clone(),
                source,
            })?;
        file.write_all(pretty.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|source| ConfigError::Io {
                operation: "write temporary config",
                path: temporary_path.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ConfigError::Io {
            operation: "sync temporary config",
            path: temporary_path.clone(),
            source,
        })?;
        fs::rename(&temporary_path, path).map_err(|source| ConfigError::Io {
            operation: "replace config",
            path: path.to_owned(),
            source,
        })?;
        // Windows cannot open a directory as a file; its rename is durable
        // enough without the extra sync.
        if cfg!(windows) {
            return Ok(());
        }
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConfigError::Io {
                operation: SYNC_DIRECTORY,
                path: parent.to_owned(),
                source,
            })
    })();

    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result
}

pub fn config_path_from_environment() -> Result<PathBuf, ConfigError> {
    #[cfg(windows)]
    return config_path_from_values(env::var_os("APPDATA"), None);
    #[cfg(not(windows))]
    config_path_from_values(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn config_path_from_values(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    Ok(config_root(xdg_config_home, home)?
        .join(APP_ID)
        .join(CONFIG_FILENAME))
}

fn config_root(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = nonempty_os(xdg_config_home) {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = nonempty_os(home) {
        return Ok(PathBuf::from(path).join(".config"));
    }
    Err(ConfigError::UnresolvedHome)
}

fn nonempty_os(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn map_read_error(path: &Path, source: io::Error) -> ConfigError {
    if source.kind() == io::ErrorKind::NotFound {
        ConfigError::NotFound(path.to_owned())
    } else {
        ConfigError::Io {
            operation: "read config",
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    NotFound(PathBuf),
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    Validation(Vec<ValidationProblem>),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    UnresolvedHome,
}

/// Naming the one post-rename step lets a caller tell "the write never
/// happened" from "the write is visible but its durability is unconfirmed".
const SYNC_DIRECTORY: &str = "sync config directory";

impl ConfigError {
    /// True when the new config is already on disk despite the error, so the
    /// caller must keep the value it just wrote rather than roll it back.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Io { operation, .. } if *operation == SYNC_DIRECTORY)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(formatter, "config not found: {}", path.display()),
            Self::InvalidJson { path, .. } => {
                write!(formatter, "invalid JSON in {}", path.display())
            }
            Self::Validation(problems) => {
                write!(
                    formatter,
                    "config has {} validation problem(s)",
                    problems.len()
                )
            }
            Self::Io {
                operation, path, ..
            } => write!(formatter, "failed to {operation}: {}", path.display()),
            Self::UnresolvedHome => formatter.write_str(
                "Cannot find the config directory because XDG_CONFIG_HOME and HOME are unset.",
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidJson { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let path = crate::storage::test_root(name);
        fs::create_dir(&path).expect("create test directory");
        path
    }

    fn ready_config() -> Config {
        let mut config = Config::default();
        config.storage.recording_dir = AuthorizedPath::authorized("/recordings");
        config.flavors.retail = FlavorConfig {
            enabled: true,
            log_dir: AuthorizedPath::authorized("/games/wow/_retail_/Logs"),
        };
        config
    }

    #[test]
    fn default_config_round_trips_atomically_with_private_permissions() {
        let directory = temporary_directory("config-round-trip");
        let path = directory.join(CONFIG_FILENAME);
        let config = Config::default();

        config.save(&path).expect("save default config");
        assert_eq!(Config::load(&path).expect("load default config"), config);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!directory.join("config.json.tmp").exists());

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn validation_reports_every_constraint_in_stable_field_order() {
        // Rules the ordered assertion below does not already exercise.
        type Mutation = Box<dyn Fn(&mut Config)>;
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "storage.recording_dir",
                Box::new(|config| config.storage.recording_dir = AuthorizedPath::unset()),
            ),
            (
                "storage.recording_dir",
                Box::new(|config| {
                    config.storage.recording_dir.authorization = PathAuthorization::Unset
                }),
            ),
            (
                "storage.recording_dir",
                Box::new(|config| {
                    config.storage.recording_dir = AuthorizedPath::authorized("recordings")
                }),
            ),
            (
                "flavors.retail",
                Box::new(|config| config.flavors.retail.log_dir = AuthorizedPath::unset()),
            ),
            (
                "flavors.retail",
                Box::new(|config| {
                    config.flavors.retail.log_dir = AuthorizedPath::authorized("/wow/Data")
                }),
            ),
            (
                "capture.audio_input",
                Box::new(|config| config.capture.audio_input = Some(String::new())),
            ),
            (
                "capture.capture_target_token",
                Box::new(|config| config.capture.capture_target_token = Some(String::new())),
            ),
        ];

        for (expected_field, mutate) in cases {
            let mut config = ready_config();
            mutate(&mut config);
            let problems = config.validate();
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.field == expected_field && !problem.message.is_empty()),
                "missing field-specific validation for {expected_field}: {problems:?}"
            );
        }

        let mut config = Config {
            version: 99,
            ..Config::default()
        };
        config.storage.recording_dir = AuthorizedPath {
            path: PathBuf::from("/recordings#old"),
            authorization: PathAuthorization::ImportedInactive,
        };
        config.capture.fps = 14;
        config.capture.bitrate_kbps = 999;
        config.capture.extra_lead_in_seconds = 31;
        config.capture.audio_output.clear();
        config.activities.min_raid_duration_seconds = 10_001;
        config.activities.raid_overrun_seconds = 61;
        config.activities.dungeon_overrun_seconds = 61;
        config.flavors.retail = FlavorConfig {
            enabled: true,
            log_dir: AuthorizedPath {
                path: PathBuf::from("/games/wow/_retail_/Logs"),
                authorization: PathAuthorization::ImportedInactive,
            },
        };
        disable_automatic_activities(&mut config);

        let fields: Vec<_> = config
            .validate()
            .into_iter()
            .map(|problem| problem.field)
            .collect();
        assert_eq!(
            fields,
            [
                "version",
                "capture.fps",
                "capture.bitrate_kbps",
                "capture.extra_lead_in_seconds",
                "capture.audio_output",
                "activities.min_raid_duration_seconds",
                "activities.raid_overrun_seconds",
                "activities.dungeon_overrun_seconds",
                "storage.recording_dir",
                "flavors.retail",
                "activities",
            ]
        );

        let mut single_path_problem = ready_config();
        single_path_problem.flavors.classic.log_dir = AuthorizedPath {
            path: PathBuf::from("/games/wow/_classic_/Logs"),
            authorization: PathAuthorization::Unset,
        };
        assert_eq!(
            single_path_problem
                .persistence_problems()
                .iter()
                .filter(|problem| problem.field == "flavors.classic")
                .count(),
            1,
            "each saved flavor path state is validated exactly once"
        );
    }

    #[test]
    fn invalid_native_json_and_failed_save_preserve_existing_file() {
        let directory = temporary_directory("config-failure");
        let invalid_path = directory.join("invalid.json");
        fs::write(&invalid_path, b"{not-json").expect("write invalid config");
        assert!(matches!(
            Config::load(&invalid_path),
            Err(ConfigError::InvalidJson { .. })
        ));
        assert_eq!(
            fs::read(directory.join("invalid.json.bad")).expect("rejected copy"),
            b"{not-json"
        );

        let path = directory.join(CONFIG_FILENAME);
        let existing = ready_config();
        existing.save(&path).expect("write existing valid config");
        let existing_bytes = fs::read(&path).expect("read existing config");
        fs::create_dir(directory.join("config.json.tmp")).expect("block temporary file creation");
        let mut changed = existing;
        changed.capture.fps = 30;
        assert!(matches!(changed.save(&path), Err(ConfigError::Io { .. })));
        // Invalid values are refused before anything is written.
        changed.capture.fps = 240;
        let Err(ConfigError::Validation(problems)) = changed.save(&path) else {
            panic!("invalid save must fail validation")
        };
        assert_eq!(problems[0].field, "capture.fps");
        assert_eq!(
            fs::read(&path).expect("read preserved config"),
            existing_bytes
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn config_paths_follow_nonempty_xdg_then_home() {
        assert_eq!(
            config_path_from_values(
                Some(OsString::from("/xdg")),
                Some(OsString::from("/home/a"))
            )
            .expect("xdg config path"),
            PathBuf::from("/xdg").join(APP_ID).join(CONFIG_FILENAME)
        );
        assert_eq!(
            config_path_from_values(Some(OsString::new()), Some(OsString::from("/home/a")))
                .expect("home config path"),
            PathBuf::from("/home/a/.config")
                .join(APP_ID)
                .join(CONFIG_FILENAME)
        );
        assert!(matches!(
            config_path_from_values(None, None),
            Err(ConfigError::UnresolvedHome)
        ));
    }

    fn disable_automatic_activities(config: &mut Config) {
        config.activities.record_raids = false;
        config.activities.record_dungeons = false;
        config.activities.record_two_v_two = false;
        config.activities.record_three_v_three = false;
        config.activities.record_five_v_five = false;
        config.activities.record_skirmish = false;
        config.activities.record_solo_shuffle = false;
        config.activities.record_battlegrounds = false;
        config.activities.record_challenge_modes = false;
    }
}
