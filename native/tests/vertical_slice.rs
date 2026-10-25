// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless vertical slice: config, log polling, activity detection, recorder
//! control, storage, and media jobs behind the real coordinator.
//!
//! The recorder and FFmpeg are replaced by fakes; every
//! other type is the production one operating on a temp directory. The
//! coordinator core is stepped with `tick()`, so no test sleeps or timing
//! assertions are needed.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use warcraft_recorder::config::{
    ActivitySettings, AuthorizedPath, CONFIG_FILENAME, CaptureSettings, Config, FlavorConfig,
    ManualSettings, StorageSettings,
};
use warcraft_recorder::coordinator::{AppSnapshot, ClipRange, Command, Coordinator, Setup, start};
use warcraft_recorder::domain::{
    Category, MeterFight, MeterMetric, Outcome, RecorderStatus, StorageLimit, TimelineKind,
};
use warcraft_recorder::media_jobs::MediaConfig;
use warcraft_recorder::recorder::Timeouts;
use warcraft_recorder::storage::{RECOVERY_DIR, load_meter, now_unix_ms};

const PLAYER_GUID: &str = "Player-1092-0A70E103";
const PLAYER_NAME: &str = "Testplayer-Testrealm";
/// Hostile boss the player's spells land on; its flags are not friendly.
const BOSS_GUID: &str = "Creature-0-3013-2820-74284-0000266503";
const BOSS_FLAGS: &str = "0x10a48";
const SELF_FLAGS: &str = "0x511";
/// Long enough for real process spawns and FFmpeg fakes, short enough to fail
/// fast. Nothing is asserted about how long a step actually takes.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

// --- Harness ---

struct Harness {
    root: PathBuf,
    library: PathBuf,
    capture_root: PathBuf,
    log_file: PathBuf,
    coordinator: Coordinator,
    commands: SyncSender<Command>,
    snapshots: Receiver<Arc<AppSnapshot>>,
    latest: Arc<AppSnapshot>,
}

impl Harness {
    fn new(name: &str) -> Self {
        let (root, library, capture_root, log_file) = spawn_tree(name);
        Self::attach(root, library, capture_root, log_file)
    }

    /// Build a coordinator over an existing directory tree.
    fn attach(root: PathBuf, library: PathBuf, capture_root: PathBuf, log_file: PathBuf) -> Self {
        let setup = setup(&root);
        Self::attach_until(setup, root, library, capture_root, log_file, armed)
    }

    fn attach_until(
        setup: Setup,
        root: PathBuf,
        library: PathBuf,
        capture_root: PathBuf,
        log_file: PathBuf,
        settled: impl FnMut(&AppSnapshot) -> bool,
    ) -> Self {
        let (commands, commands_rx) = mpsc::sync_channel(64);
        let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
        let config = Config::load(&setup.config_path);
        let mut coordinator =
            Coordinator::new(setup, config, commands_rx, snapshot_tx, Box::new(|| {}));
        coordinator.startup();
        let mut harness = Self {
            root,
            library,
            capture_root,
            log_file,
            coordinator,
            commands,
            snapshots,
            latest: Arc::new(empty_snapshot()),
        };
        harness.pump(settled);
        harness
    }

    /// Step the coordinator until the condition holds. Panics on timeout so a
    /// broken flow fails loudly instead of asserting on a stale snapshot.
    fn pump(&mut self, mut done: impl FnMut(&AppSnapshot) -> bool) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            self.coordinator.tick();
            while let Ok(snapshot) = self.snapshots.try_recv() {
                self.latest = snapshot;
            }
            if done(&self.latest) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; status {:?}, problems {:?}",
                self.latest.status,
                self.latest.problems
            );
        }
    }

    /// Tear the coordinator down and rebuild over the same tree.
    fn restart(self) -> Self {
        let (root, library, capture_root, log_file) = (
            self.root.clone(),
            self.library.clone(),
            self.capture_root.clone(),
            self.log_file.clone(),
        );
        drop(self);
        Self::attach(root, library, capture_root, log_file)
    }

    fn send(&self, command: Command) {
        assert!(
            self.commands.try_send(command).is_ok(),
            "command queue full"
        );
    }

    fn log(&self, lines: &[String]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&self.log_file)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn entries_of(&self, category: &Category) -> Vec<&warcraft_recorder::domain::LibraryEntry> {
        self.latest
            .entries
            .iter()
            .filter(|entry| &entry.category == category)
            .collect()
    }
}

/// Startup publishes the scanned library before it arms, so settle on the
/// post-arm snapshot rather than the first one out.
fn armed(snapshot: &AppSnapshot) -> bool {
    !matches!(
        snapshot.status,
        RecorderStatus::SetupRequired | RecorderStatus::WaitingForWow
    )
}

fn setup(root: &Path) -> Setup {
    Setup {
        config_path: root.join(CONFIG_FILENAME),
        data_dir: root.join("recorder"),
        ipc_socket: root.join("recorder/gsr.sock"),
        gsr_binary: fixture_bin("fake-gsr.py"),
        media: MediaConfig {
            ffmpeg: fixture_bin("fake-ffmpeg.sh"),
            utc_offset_minutes: 0,
            poll_interval: Duration::from_millis(10),
            finalize_grace: Duration::from_secs(5),
            sigint_grace: Duration::from_millis(300),
        },
        year: current_year(),
        recorder_timeouts: Timeouts {
            arm_stability: Duration::from_millis(150),
            replay_reply: Duration::from_millis(400),
            // Ends are asynchronous, so this budget spans however long a test
            // holds the fake's stop reply, including `Config::save`'s two
            // fsyncs. Keep it far enough above that work that a loaded
            // filesystem cannot expire it: the failure mode is a dropped
            // recording and a 20 s `pump` timeout, not a clear assertion.
            stop_reply: Duration::from_secs(2),
            exit_grace: Duration::from_millis(500),
        },
        wow_check_interval: Duration::from_millis(50),
        poll_interval: Duration::from_millis(5),
        test_duration: Duration::from_millis(200),
    }
}

/// A fresh temp tree: empty directories, an empty combat log, and a config
/// pointing at them.
fn spawn_tree(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("wr-slice-{name}-{}", uuid::Uuid::new_v4()));
    let library = root.join("recordings with space");
    let capture_root = root.join("buffer");
    let log_dir = root.join("wow/_retail_/Logs");
    for directory in [&library, &capture_root, &log_dir] {
        fs::create_dir_all(directory).unwrap();
    }
    let log_file = log_dir.join("WoWCombatLog.txt");
    fs::write(&log_file, b"").unwrap();
    write_config(&root, &library, &capture_root, &log_dir);
    (root, library, capture_root, log_file)
}

fn fixture_bin(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/native/bin")
        .join(name)
}

fn write_config(root: &Path, library: &Path, capture_root: &Path, log_dir: &Path) {
    let config = Config {
        flavors: warcraft_recorder::config::FlavorSettings {
            retail: FlavorConfig {
                enabled: true,
                log_dir: AuthorizedPath::authorized(log_dir),
            },
            ..Default::default()
        },
        activities: ActivitySettings {
            min_raid_duration_seconds: 0,
            raid_overrun_seconds: 0,
            dungeon_overrun_seconds: 0,
            ..Default::default()
        },
        storage: StorageSettings {
            recording_dir: AuthorizedPath::authorized(library),
            separate_buffer_dir: true,
            buffer_dir: AuthorizedPath::authorized(capture_root),
            limit: StorageLimit::Unlimited,
        },
        manual: ManualSettings {
            enabled: true,
            ..Default::default()
        },
        capture: CaptureSettings {
            extra_lead_in_seconds: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    config.save(&root.join(CONFIG_FILENAME)).unwrap();
}

fn empty_snapshot() -> AppSnapshot {
    AppSnapshot {
        entries: Arc::new(Vec::new()),
        correlations: Arc::new(Vec::new()),
        category_counts: Vec::new(),
        status: RecorderStatus::SetupRequired,
        active: None,
        config: Config::default(),
        setup_problems: Vec::new(),
        advanced_logging: Vec::new(),
        problems: Vec::new(),
        work: None,
        queued_jobs: 0,
        storage_used_bytes: 0,
        protected_over_limit: false,
    }
}

// --- Combat-log helpers ---

/// `M/D HH:MM:SS.mmm` in UTC, matching the harness's zero UTC offset.
fn stamp(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (_, month, day) = civil_from_days(days);
    format!(
        "{}/{} {:02}:{:02}:{:02}.{:03}",
        month,
        day,
        ms_of_day / 3_600_000,
        (ms_of_day / 60_000) % 60,
        (ms_of_day / 1_000) % 60,
        ms_of_day % 1_000
    )
}

fn current_year() -> i32 {
    civil_from_days(now_unix_ms().div_euclid(86_400_000)).0
}

/// Howard Hinnant's `civil_from_days`, the inverse of the parser's conversion.
fn civil_from_days(days: i64) -> (i32, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    ((year + i64::from(month <= 2)) as i32, month, day)
}

fn line(at_ms: i64, payload: &str) -> String {
    format!("{}  {payload}", stamp(at_ms))
}

/// The combatant plus self-cast pair every activity needs before its metadata
/// is complete.
fn player_lines(at_ms: i64) -> Vec<String> {
    vec![
        line(
            at_ms,
            &format!(
                "COMBATANT_INFO,{PLAYER_GUID},0,1,1,1,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,577"
            ),
        ),
        line(
            at_ms,
            &format!(
                "SPELL_AURA_APPLIED,{PLAYER_GUID},\"{PLAYER_NAME}\",{SELF_FLAGS},0x0,\
                 {PLAYER_GUID},\"{PLAYER_NAME}\",{SELF_FLAGS},0x0,1,\"Test Aura\",0x1,BUFF"
            ),
        ),
    ]
}

fn raid_start(at_ms: i64) -> Vec<String> {
    let mut lines = vec![
        line(
            at_ms,
            "COMBAT_LOG_VERSION,22,ADVANCED_LOG_ENABLED,1,BUILD_VERSION,12.1.0,PROJECT_ID,1",
        ),
        line(at_ms, "ENCOUNTER_START,2820,\"Test Encounter\",16,20,2549"),
    ];
    lines.extend(player_lines(at_ms));
    lines
}

fn raid_end(at_ms: i64, success: bool) -> String {
    line(
        at_ms,
        &format!(
            "ENCOUNTER_END,2820,\"Test Encounter\",16,20,{}",
            u8::from(success)
        ),
    )
}

fn player_death(at_ms: i64) -> String {
    line(
        at_ms,
        &format!(
            "UNIT_DIED,0000000000000000,nil,0x80000000,0x80000000,\
             {PLAYER_GUID},\"{PLAYER_NAME}\",{SELF_FLAGS},0x0"
        ),
    )
}

/// The player's spell on the hostile boss in the retail advanced-block shape.
/// The tiny HP values keep the destination below the boss-health floor, so the
/// existing boss-health behavior is untouched.
fn spell_damage(at_ms: i64) -> String {
    line(
        at_ms,
        &format!(
            "SPELL_DAMAGE,{PLAYER_GUID},\"{PLAYER_NAME}\",{SELF_FLAGS},0x0,{BOSS_GUID},\"Test Boss\",\
             {BOSS_FLAGS},0x0,585,\"Smite\",0x2,{BOSS_GUID},0000000000000000,105,152,0,0,189,2084,0,0,0,\
             0,0,0,0,0,0,0,0,1500,0,2,0,0,0,1,0,0,0,0.000,1,1"
        ),
    )
}

/// A self-heal in the modern suffix layout; 400 of the 1000 points overheat.
fn spell_heal(at_ms: i64) -> String {
    line(
        at_ms,
        &format!(
            "SPELL_HEAL,{PLAYER_GUID},\"{PLAYER_NAME}\",{SELF_FLAGS},0x0,{PLAYER_GUID},\"{PLAYER_NAME}\",\
             {SELF_FLAGS},0x0,2061,\"Flash Heal\",0x2,{PLAYER_GUID},0000000000000000,500,500,0,0,0,0,0,0,0,0,\
             0,0,0,0,0,0,0,1000,600,400,0,1"
        ),
    )
}

// --- Scenarios ---

#[test]
fn automatic_raid_completes_and_survives_a_restart() {
    let mut harness = Harness::new("raid");
    assert_eq!(harness.latest.status, RecorderStatus::Ready);

    let start_ms = now_unix_ms() - 2_000;
    harness.log(&raid_start(start_ms));
    harness.pump(|snapshot| snapshot.active.is_some());
    let active = harness.latest.active.clone().unwrap();
    // Detection is the encounter start itself, so the request is the extra
    // lead-in alone, clamped to the buffer.
    assert_eq!(active.requested_replay_ms, 10_000);
    assert_eq!(active.category, Category::Raids);

    harness.log(&[
        spell_damage(start_ms + 250),
        spell_heal(start_ms + 300),
        spell_damage(start_ms + 1_250),
    ]);
    harness.log(&[player_death(start_ms + 1_500)]);
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());

    let entry = harness.latest.entries[0].clone();
    assert_eq!(entry.category, Category::Raids);
    assert_eq!(entry.outcome, Outcome::Win);
    assert!(entry.media_path.exists(), "media was not written");
    assert!(entry.sidecar_path.exists(), "sidecar was not written");
    // The replay is in front of the media, so the death sits later than its
    // 500 ms activity offset and still inside the media.
    let death = entry
        .timeline
        .iter()
        .find(|item| item.kind() == &TimelineKind::Death)
        .expect("death marker");
    assert!(
        death.start_ms() > 500 && death.start_ms() <= entry.duration_ms,
        "death at {} ms, duration {} ms",
        death.start_ms(),
        entry.duration_ms
    );
    // The intermediates were consumed by finalization.
    assert!(read_dir_count(&harness.capture_root.join("replay")) == 0);
    assert!(read_dir_count(&harness.capture_root.join("regular")) == 0);
    // The version-22 advanced-layout events survived parsing into per-second
    // deltas while retaining their exact full-fight aggregates.
    let meter = load_meter(&entry.sidecar_path).unwrap();
    let fights = &meter.fights;
    assert_eq!(fights.len(), 1, "expected exactly one meter fight");
    assert_eq!(meter_total(&fights[0], MeterMetric::Damage), 3_000);
    assert_eq!(meter_total(&fights[0], MeterMetric::Healing), 600);
    let damage = fights[0].actors[0]
        .spells
        .iter()
        .find(|entry| entry.metric == MeterMetric::Damage)
        .expect("damage spell");
    assert_eq!(damage.samples.len(), 2);
    assert_eq!(
        damage
            .samples
            .iter()
            .map(|sample| sample.amount)
            .collect::<Vec<_>>(),
        vec![1_500, 1_500]
    );
    assert!(damage.samples[0].at_ms < damage.samples[1].at_ms);
    assert!(
        damage
            .samples
            .iter()
            .all(|sample| sample.at_ms <= entry.duration_ms)
    );

    // Tag and protect go through the real sidecar.
    harness.send(Command::SetTag {
        id: entry.id.clone(),
        tag: "  keeper  ".to_owned(),
    });
    harness.send(Command::SetProtected {
        ids: vec![entry.id.clone()],
        value: true,
    });
    harness.pump(|snapshot| {
        snapshot
            .entries
            .first()
            .is_some_and(|entry| entry.protected && entry.tag.as_deref() == Some("  keeper  "))
    });

    // Restart: a stray artifact is quarantined and the entry is rescanned.
    let stray = harness.capture_root.join("regular/Video_stray.mkv");
    fs::write(&stray, b"interrupted").unwrap();
    let mut restarted = harness.restart();
    assert_eq!(restarted.latest.entries.len(), 1);
    assert!(restarted.latest.entries[0].protected);
    assert!(!stray.exists(), "stray artifact was not swept");
    assert!(
        read_dir_count(&restarted.library.join(RECOVERY_DIR)) > 0,
        "nothing was quarantined"
    );

    // The tagged, protected, and rescanned sidecar still carries the same
    // full and per-second aggregates.
    let reloaded = load_meter(&restarted.latest.entries[0].sidecar_path).unwrap();
    assert_eq!(reloaded, meter);

    let id = restarted.latest.entries[0].id.clone();
    restarted.send(Command::Delete { ids: vec![id] });
    restarted.pump(|snapshot| snapshot.entries.is_empty());
}

#[test]
fn force_ended_solo_shuffle_is_abandoned_and_saved() {
    let mut harness = Harness::new("shuffle");
    let start_ms = now_unix_ms() - 1_000;
    let mut lines = vec![line(
        start_ms,
        "ARENA_MATCH_START,2547,33,Rated Solo Shuffle,1",
    )];
    lines.extend(player_lines(start_ms));
    harness.log(&lines);
    harness.pump(|snapshot| snapshot.active.is_some());
    assert_eq!(
        harness.latest.active.as_ref().unwrap().category,
        Category::SoloShuffle
    );

    harness.send(Command::ForceEnd);
    harness.pump(|snapshot| !snapshot.entries.is_empty());

    let entry = &harness.latest.entries[0];
    assert_eq!(entry.category, Category::SoloShuffle);
    assert_eq!(entry.outcome, Outcome::Loss);
    assert!(
        entry
            .timeline
            .iter()
            .any(|item| item.kind() == &TimelineKind::Round),
        "expected the unended round marker"
    );
}

#[test]
fn manual_and_test_recordings_reuse_the_capture_pipeline() {
    let mut harness = Harness::new("manual");

    harness.send(Command::StartManual);
    harness.pump(|snapshot| snapshot.active.is_some());
    assert_eq!(
        harness.latest.active.as_ref().unwrap().category,
        Category::Manual
    );
    harness.send(Command::StopManual);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let manual = harness.latest.entries[0].clone();
    assert_eq!(manual.category, Category::Manual);
    assert_eq!(manual.title, "Manual recording");

    // The test recording injects its own start and end.
    harness.send(Command::RunTest {
        category: Category::Raids,
    });
    harness.pump(|snapshot| {
        matches!(
            snapshot.status,
            RecorderStatus::Recording {
                test: true,
                manual: false,
                ..
            }
        )
    });
    harness.pump(|snapshot| snapshot.entries.len() == 2);
    let raid = load_meter(&harness.entries_of(&Category::Raids)[0].sidecar_path).unwrap();
    assert_eq!(raid.fights.len(), 1);
    assert_eq!(meter_total(&raid.fights[0], MeterMetric::Damage), 7_800_000);
    assert_eq!(raid.fights[0].actors.len(), 2);
}

#[test]
fn finalization_precedes_queued_user_jobs() {
    let mut harness = Harness::new("queue");

    // One finished recording to clip against.
    let start_ms = now_unix_ms() - 1_000;
    harness.log(&raid_start(start_ms));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let source = harness.latest.entries[0].clone();

    // A second recording, then complete it and queue a clip before the tick's
    // single dispatch: both jobs are queued before dispatch chooses a job.
    harness.log(&raid_start(now_unix_ms() - 1_000));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.send(Command::CreateClip(ClipRange {
        source: source.id.clone(),
        start_ms: 0,
        end_ms: source.duration_ms.min(2_000),
    }));

    let mut order: Vec<Category> = Vec::new();
    harness.pump(|snapshot| {
        for entry in snapshot.entries.iter() {
            if !order.contains(&entry.category) && entry.id != source.id {
                order.push(entry.category.clone());
            }
        }
        order.len() == 2
    });
    assert_eq!(order, vec![Category::Raids, Category::Clip]);
}

#[test]
fn commands_are_served_while_a_capture_is_ending() {
    let mut harness = Harness::new("ending");
    harness.log(&raid_start(now_unix_ms() - 1_000));
    harness.pump(|snapshot| snapshot.active.is_some());

    // End the activity while the fake holds back the stop reply.
    let hold = harness.root.join("recorder/fake-hold");
    fs::write(&hold, b"").unwrap();
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| snapshot.active.is_none());
    assert!(
        matches!(harness.latest.status, RecorderStatus::Finalizing { .. }),
        "status {:?}",
        harness.latest.status
    );
    assert!(harness.latest.entries.is_empty());

    harness.send(Command::SetSelectedCategory {
        category: Category::MythicPlus,
    });
    harness.pump(|snapshot| snapshot.config.interface.selected_category == Category::MythicPlus);
    assert!(
        harness.latest.entries.is_empty(),
        "the capture must still be waiting on its artifacts"
    );

    // The artifacts finally land: the recording finalizes as usual.
    fs::remove_file(&hold).unwrap();
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    assert_eq!(harness.latest.entries[0].category, Category::Raids);
}

/// A new pull without an `ENCOUNTER_END` for the previous one supersedes it:
/// both must be saved, the old one abandoned.
#[test]
fn a_superseding_encounter_keeps_both_pulls() {
    let mut harness = Harness::new("superseded");
    harness.log(&raid_start(now_unix_ms() - 1_000));
    harness.pump(|snapshot| snapshot.active.is_some());
    let first = harness.latest.active.clone().unwrap().id;

    harness.log(&raid_start(now_unix_ms()));
    harness.pump(|snapshot| {
        snapshot
            .active
            .as_ref()
            .is_some_and(|active| active.id != first)
    });
    let second = harness.latest.active.clone().unwrap().id;
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);

    let outcome_of = |id| {
        harness
            .latest
            .entries
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| entry.outcome)
    };
    assert_eq!(outcome_of(&first), Some(Outcome::Loss));
    assert_eq!(outcome_of(&second), Some(Outcome::Win));
}

#[test]
fn dismissing_the_release_notes_ends_them_for_good() {
    let harness = Harness::new("dismissals");
    let config_path = harness.root.join(CONFIG_FILENAME);
    let mut config = Config::load(&config_path).expect("load the harness config");
    // What an install updated from an earlier version looks like: the field
    // is missing from its config file, so it deserializes empty.
    config.last_seen_version = String::new();
    config
        .save(&config_path)
        .expect("seed the pending release notes");
    let mut harness = harness.restart();
    assert!(harness.latest.config.last_seen_version.is_empty());

    harness.send(Command::DismissReleaseNotes);
    harness.pump(|snapshot| snapshot.config.last_seen_version == warcraft_recorder::VERSION);
    let saved = Config::load(&config_path).expect("reload the saved config");
    assert_eq!(
        saved.last_seen_version,
        warcraft_recorder::VERSION,
        "the dismissal must outlive the process"
    );

    // Dialogs opened before the notes were closed carry a draft with the old
    // values. Applying it must not replay the notes, or they reappear on
    // every later start.
    let mut stale = harness.latest.config.clone();
    stale.last_seen_version = String::new();
    stale.capture.fps = 30;
    harness.send(Command::SaveConfig {
        draft: Box::new(stale),
    });
    harness.pump(|snapshot| snapshot.config.capture.fps == 30);
    assert_eq!(
        harness.latest.config.last_seen_version,
        warcraft_recorder::VERSION,
        "a settings save must not resurrect the dismissal"
    );
    let saved = Config::load(&config_path).expect("reload after the settings save");
    assert_eq!(
        saved.last_seen_version,
        warcraft_recorder::VERSION,
        "the resurrected notes must not reach disk either"
    );
}

#[test]
fn replay_buffer_waits_for_wow_to_run() {
    let (root, library, capture_root, log_file) = spawn_tree("standby");
    let config_path = root.join(CONFIG_FILENAME);
    let mut config = Config::load(&config_path).unwrap();
    config.capture.capture_target_token = Some("token".to_owned());
    config.save(&config_path).unwrap();
    // WoW last wrote its Logs folder an hour ago.
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3_600);
    fs::File::options()
        .write(true)
        .open(&log_file)
        .unwrap()
        .set_modified(an_hour_ago)
        .unwrap();
    let log_dir = log_file.parent().unwrap().to_owned();
    let setup = setup(&root);
    let mut harness =
        Harness::attach_until(setup, root, library, capture_root, log_file, |snapshot| {
            snapshot.status == RecorderStatus::WaitingForWow
        });
    for _ in 0..20 {
        harness.coordinator.tick();
    }
    assert_eq!(harness.latest.status, RecorderStatus::WaitingForWow);

    // Launching WoW rewrites its client logs; the next check arms.
    fs::write(log_dir.join("gx.log"), b"App is foreground\n").unwrap();
    harness.pump(|snapshot| snapshot.status == RecorderStatus::Ready);
}

#[test]
fn missing_regular_artifact_is_reported_and_the_next_capture_saves() {
    let mut harness = Harness::new("failure");
    let no_regular = harness.root.join("recorder/fake-no-regular");
    fs::write(&no_regular, b"").unwrap();
    harness.log(&raid_start(now_unix_ms() - 1_000));
    harness.pump(|snapshot| snapshot.active.is_some());
    // GSR refuses the stop, so there is no regular recording.
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| !snapshot.problems.is_empty());
    assert!(harness.latest.entries.is_empty());
    assert!(
        harness
            .latest
            .problems
            .iter()
            .any(|problem| problem.summary.contains("no video file")),
        "problems: {:?}",
        harness.latest.problems
    );
    // The same child serves the next recording.
    fs::remove_file(&no_regular).unwrap();
    harness.pump(|snapshot| snapshot.status == RecorderStatus::Ready);
    harness.log(&raid_start(now_unix_ms() - 1_000));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| !snapshot.entries.is_empty());
    let entry = &harness.latest.entries[0];
    assert_eq!(entry.category, Category::Raids);
    assert!(entry.media_path.exists(), "media was not written");
}

/// A harness over a tree that already holds one scanned recording
/// (`canary-old`), optionally protected. Its media is removed right after
/// startup, so any full library rescan would drop it: the canary stays in the
/// index only while completion, mutation, and eviction update incrementally.
fn canary_harness(
    name: &str,
    protected: bool,
) -> (Harness, warcraft_recorder::domain::LibraryEntry) {
    let (root, library, capture_root, log_file) = spawn_tree(name);
    let start = now_unix_ms() - 61_000;
    // A native sidecar, exactly as finalize would write one: the canary must
    // scan like any recording the app itself produced.
    let sidecar = format!(
        r#"{{"schema_version":1,"media_file":"canary-old.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","category":"raids","flavor":"retail","title":"Canary","start_unix_ms":{start},"duration_ms":60000,"outcome":"unknown","protected":{protected},"combatants":[],"timeline":[],"details":{{"kind":"raid"}},"media":{{"has_content":true}}}}"#
    );
    fs::write(library.join("canary-old.json"), sidecar).unwrap();
    fs::write(library.join("canary-old.mp4"), b"canary media").unwrap();

    let harness = Harness::attach(root, library, capture_root, log_file);
    assert_eq!(harness.latest.entries.len(), 1);
    let canary = harness.latest.entries[0].clone();
    fs::remove_file(&canary.media_path).unwrap();
    (harness, canary)
}

/// Media plus sidecar bytes for every entry still counted by the coordinator.
fn counted_usage(entries: &[&warcraft_recorder::domain::LibraryEntry]) -> u64 {
    entries
        .iter()
        .flat_map(|entry| [&entry.media_path, &entry.sidecar_path])
        .map(|path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
        .sum()
}

#[test]
fn completion_updates_the_library_without_a_full_rescan() {
    let (mut harness, canary) = canary_harness("incremental", false);

    let start_ms = now_unix_ms() - 1_000;
    harness.log(&raid_start(start_ms));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);

    // The completed recording is the newest entry and the canary kept its
    // place despite its missing media, which a rescan would have dropped.
    let raid = harness.latest.entries[0].clone();
    assert_eq!(raid.category, Category::Raids);
    assert_eq!(harness.latest.entries[1].id, canary.id);
    // Correlation groups follow the same order.
    assert_eq!(harness.latest.correlations.len(), 2);
    assert_eq!(harness.latest.correlations[0].primary_id, raid.id);
    assert_eq!(harness.latest.correlations[1].primary_id, canary.id);
    // Usage tracks the real files, including the canary's missing media.
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&raid, &canary])
    );

    // Tag and protect fold into the index the same way.
    harness.send(Command::SetTag {
        id: raid.id.clone(),
        tag: "keeper".to_owned(),
    });
    harness.pump(|snapshot| snapshot.entries[0].tag.as_deref() == Some("keeper"));
    assert_eq!(harness.latest.entries[1].id, canary.id);

    // Deletion removes only the deleted entry.
    harness.send(Command::Delete {
        ids: vec![raid.id.clone()],
    });
    harness.pump(|snapshot| snapshot.entries.len() == 1);
    assert_eq!(harness.latest.entries[0].id, canary.id);
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&harness.latest.entries[0]])
    );
    assert!(!raid.media_path.exists());
    assert!(!raid.sidecar_path.exists());
}

#[test]
fn completion_eviction_updates_the_index_without_a_full_rescan() {
    let (mut harness, canary) = canary_harness("eviction", true);

    // One unprotected raid, then a storage cap below its padded size.
    let start_ms = now_unix_ms() - 1_000;
    harness.log(&raid_start(start_ms));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    harness.pump(|snapshot| snapshot.entries.len() == 2);
    let raid = harness.latest.entries[0].clone();
    assert_eq!(raid.category, Category::Raids);

    let mut draft = harness.latest.config.clone();
    draft.storage.limit = StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"));
    harness.send(Command::SaveConfig {
        draft: Box::new(draft),
    });
    harness.pump(|snapshot| {
        snapshot.config.storage.limit == StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"))
    });

    // Sparse-pad the raid past the cap without touching its bytes.
    let padded = fs::OpenOptions::new()
        .write(true)
        .open(&raid.media_path)
        .unwrap();
    padded.set_len(2 * 1024 * 1024 * 1024).unwrap();

    harness.log(&raid_start(now_unix_ms()));
    harness.pump(|snapshot| snapshot.active.is_some());
    harness.log(&[raid_end(now_unix_ms(), true)]);
    let raid_id = raid.id.clone();
    harness.pump(|snapshot| {
        snapshot.entries.len() == 2 && snapshot.entries.iter().all(|entry| entry.id != raid_id)
    });

    // Eviction removed the oldest unprotected recording; the protected canary
    // survived and, with its media missing, only if no rescan ran.
    let newest = harness.latest.entries[0].clone();
    assert_eq!(newest.category, Category::Raids);
    assert_ne!(newest.id, raid.id);
    assert_eq!(harness.latest.entries[1].id, canary.id);
    assert!(!raid.media_path.exists(), "evicted media was not removed");
    assert_eq!(harness.latest.correlations.len(), 2);
    assert_eq!(
        harness.latest.storage_used_bytes,
        counted_usage(&[&newest, &canary])
    );
}

#[test]
fn production_handle_starts_and_shuts_down() {
    let (root, ..) = spawn_tree("handle");

    let setup = setup(&root);
    let config = Config::load(&setup.config_path);
    let mut handle = start(setup, config, Box::new(|| {}));
    let mut snapshot = handle.snapshots.recv_timeout(STEP_TIMEOUT);
    while let Ok(current) = &snapshot {
        if current.status == RecorderStatus::Ready {
            break;
        }
        snapshot = handle.snapshots.recv_timeout(STEP_TIMEOUT);
    }
    assert_eq!(
        snapshot.expect("armed snapshot").status,
        RecorderStatus::Ready
    );
    assert!(handle.send(Command::Disarm));
    handle.shutdown();
}

/// Actor totals derive structurally from the spell entries.
fn meter_total(fight: &MeterFight, metric: MeterMetric) -> u64 {
    fight
        .actors
        .iter()
        .flat_map(|actor| &actor.spells)
        .filter(|entry| entry.metric == metric)
        .map(|entry| entry.amount)
        .sum()
}

fn read_dir_count(path: &Path) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.count())
        .unwrap_or(0)
}
