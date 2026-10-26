// SPDX-License-Identifier: GPL-3.0-or-later

//! Coordinator-owned application state and commands.
//!
//! One thread owns `Config`, the log tailers, the activity engine, the
//! `Recorder`, the library index, and the in-flight recording draft. The GTK
//! thread holds a `CoordinatorHandle`: one bounded command sender, one
//! capacity-one snapshot receiver, and one capacity-one stopped receiver. A
//! second thread runs the serial `MediaWorker`; the coordinator owns its join
//! handle and dispatches at most one job at a time so automatic finalization
//! always precedes queued user transcodes.
//!
//! Notes:
//! - Advanced-combat-logging status is read from `<log dir>/../WTF/Config.wtf`
//!   when the tailers are (re)opened; it refreshes on arm/save.
//! - Test recordings synthesize the minimum parsed events for the chosen
//!   category. `ForceEnd` stops a running test.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::activity::{ActivityAction, ActivityEngine, RecordingDraft};
use crate::config::{Config, ConfigError, LayoutSettings, ValidationProblem};
use crate::domain::{
    ActivityDetails, Category, CorrelatedActivity, DeathMarkerVisibility, GameFlavor, LibraryEntry,
    MarkerVisibility, MediaFacts, MeterData, Outcome, Problem, RecorderStatus, RecordingId,
    RecoveryAction, WorkKind, WorkProgress,
};
use crate::logwatch::LogTailer;
use crate::media_jobs::{MediaConfig, MediaControl, MediaEvent, MediaJob, MediaWorker};
use crate::parser::{CombatEvent, ParseTimeContext, ParsedEvent, PlayerObservationKind};
use crate::recorder::{
    CaptureArtifacts, CaptureConfig, Recorder, RecorderError, RecorderEvent, RecordingMode,
    StartRequest, Timeouts,
};
use crate::storage::{EntryUpdate, LibraryIndex, Storage, now_unix_ms};

/// Force-end an automatic recording after this long without new log data: a
/// crash/alt-F4 safety net only. WoW flushes the combat log in bursts, so the
/// window must sit well above that cadence or a live activity gets force-ended
/// inside a flush gap and discarded before its player is identified.
const RETAIL_DATA_TIMEOUT_MS: i64 = 10 * 60_000;
const CLASSIC_DATA_TIMEOUT_MS: i64 = 2 * 60_000;
/// WoW counts as running while anything in its Logs folder was written this
/// recently; `gx.log` alone gets a GPU status report every 5 minutes.
const WOW_IDLE_MS: i64 = 10 * 60_000;
const WOW_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Commands handled per tick before the loop returns to polling.
const COMMAND_BATCH: usize = 16;
/// Bounded problem list surfaced in the snapshot.
const MAX_PROBLEMS: usize = 8;
const CAPTURE_STOPPED_PROBLEM: &str = "Screen capture stopped unexpectedly.";
const CAPTURE_RESTART_FAILED_PROBLEM: &str = "Screen capture could not be restarted.";
/// Keep queued transcodes finite while preserving the one-worker design.
const MAX_MEDIA_QUEUE: usize = 16;

/// How long quitting waits for gpu-screen-recorder to finish writing the
/// capture it was asked to stop. Long enough for a normal flush, short enough
/// that closing the window never looks hung.
const QUIT_END_GRACE: Duration = Duration::from_secs(5);

// --- Public interface ---

/// Ranges are milliseconds into the source recording's media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipRange {
    pub source: RecordingId,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Arm,
    Disarm,
    ForceEnd,
    StartManual,
    StopManual,
    RunTest {
        category: Category,
    },
    ReselectCaptureTarget,
    SaveConfig {
        draft: Box<Config>,
    },
    SetProtected {
        ids: Vec<RecordingId>,
        value: bool,
    },
    SetTag {
        id: RecordingId,
        tag: String,
    },
    Delete {
        ids: Vec<RecordingId>,
    },
    CreateClip(ClipRange),
    SetSelectedCategory {
        category: Category,
    },
    SetMarkerVisibility {
        deaths: DeathMarkerVisibility,
        encounters: MarkerVisibility,
        rounds: MarkerVisibility,
    },
    /// Debounced UI geometry write from the shell: divider and column widths.
    SaveLayout {
        layout: LayoutSettings,
    },
    /// The user closed the "What's new" dialog for the running version.
    DismissReleaseNotes,
    Shutdown,
}

/// The in-flight recording, as the UI needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveRecordingView {
    pub id: RecordingId,
    pub category: Category,
    pub title: String,
    pub mode: RecordingMode,
    /// Wall-clock anchor for the elapsed-time display.
    pub started_unix_ms: i64,
    pub requested_replay_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppSnapshot {
    pub entries: Arc<Vec<LibraryEntry>>,
    pub correlations: Arc<Vec<CorrelatedActivity>>,
    pub category_counts: Vec<(Category, usize)>,
    pub status: RecorderStatus,
    pub active: Option<ActiveRecordingView>,
    pub config: Config,
    pub setup_problems: Vec<ValidationProblem>,
    /// One entry per enabled flavour: its config field name and whether
    /// advanced combat logging is on. `None` when `Config.wtf` could not be
    /// read, which is the normal case in the sandbox: the portal exports the
    /// chosen Logs folder alone, never its `WTF` sibling.
    pub advanced_logging: Vec<(&'static str, Option<bool>)>,
    pub problems: Vec<Problem>,
    pub work: Option<WorkProgress>,
    pub queued_jobs: usize,
    pub storage_used_bytes: u64,
    pub protected_over_limit: bool,
}

/// GTK-side handle. Dropping it closes the command channel, which stops the
/// coordinator on its next tick.
pub struct CoordinatorHandle {
    commands: Option<SyncSender<Command>>,
    pub snapshots: Receiver<Arc<AppSnapshot>>,
    pub stopped: Receiver<()>,
    join: Option<JoinHandle<()>>,
}

impl CoordinatorHandle {
    /// Returns `false` when the queue is full or the coordinator is gone; the
    /// caller shows one Busy problem rather than blocking the GTK thread.
    pub fn send(&self, command: Command) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|commands| commands.try_send(command).is_ok())
    }

    /// Request shutdown and join the coordinator thread. Takes `&mut self` so
    /// `main` can drive it through the shared handle after the GTK loop exits;
    /// `join.take()` keeps the `Drop` guard from joining twice.
    pub fn shutdown(&mut self) {
        self.request_shutdown();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    fn request_shutdown(&mut self) {
        if let Some(commands) = self.commands.take() {
            // Dropping the sender after the best-effort command makes a full
            // queue safe too: the receiver observes disconnection after it
            // drains the already queued commands and exits.
            let _ = commands.try_send(Command::Shutdown);
        }
    }
}

impl Drop for CoordinatorHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.request_shutdown();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

/// Everything the coordinator needs that is not user configuration.
#[derive(Clone, Debug)]
pub struct Setup {
    pub config_path: PathBuf,
    /// App-private directory for the recorder's token and log files.
    pub data_dir: PathBuf,
    /// gpu-screen-recorder's IPC socket.
    pub ipc_socket: PathBuf,
    pub gsr_binary: PathBuf,
    pub media: MediaConfig,
    /// Year used to expand the combat log's month/day timestamps.
    pub year: i32,
    pub recorder_timeouts: Timeouts,
    /// How often the log folder is checked for a running WoW.
    pub wow_check_interval: Duration,
    /// Idle pacing for one coordinator tick.
    pub poll_interval: Duration,
    /// Test-recording length; raids run four times as long.
    pub test_duration: Duration,
}

impl Setup {
    pub fn from_environment() -> Result<Self, ConfigError> {
        let (year, utc_offset_minutes) = local_clock();
        let config_path = crate::config::config_path_from_environment()?;
        let data_dir = config_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("recorder");
        Ok(Self {
            ipc_socket: ipc_socket_path(&data_dir),
            data_dir,
            config_path,
            // Windows resolves a bare name in the app's own folder first.
            gsr_binary: PathBuf::from(if cfg!(windows) {
                "wr-capture"
            } else {
                "gpu-screen-recorder"
            }),
            media: MediaConfig {
                utc_offset_minutes,
                ..MediaConfig::default()
            },
            year,
            recorder_timeouts: Timeouts::default(),
            wow_check_interval: WOW_CHECK_INTERVAL,
            poll_interval: Duration::from_millis(50),
            test_duration: Duration::from_secs(5),
        })
    }
}

/// gpu-screen-recorder's IPC socket. Flatpak gives each app a private runtime
/// directory; `data_dir` is the last resort because a Unix socket path must
/// stay within 107 bytes.
fn ipc_socket_path(data_dir: &Path) -> PathBuf {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return data_dir.join("gsr.sock");
    };
    match std::env::var_os("FLATPAK_ID") {
        Some(id) => runtime.join("app").join(id).join("gsr.sock"),
        None => runtime.join("warcraft-recorder-gsr.sock"),
    }
}

/// Start the coordinator and its media worker. `wake` runs whenever a new
/// snapshot or the stopped signal has been queued, so the shell can react
/// immediately instead of waiting out its next poll. It is called from the
/// coordinator thread and must be cheap and nonblocking. `config` is the
/// result of loading `setup.config_path`, done once by the caller.
pub fn start(
    setup: Setup,
    config: Result<Config, ConfigError>,
    wake: Box<dyn Fn() + Send>,
) -> CoordinatorHandle {
    let (commands_tx, commands_rx) = mpsc::sync_channel(64);
    let (snapshot_tx, snapshots) = mpsc::sync_channel(1);
    let (stopped_tx, stopped) = mpsc::sync_channel(1);
    let join = std::thread::Builder::new()
        .name("coordinator".to_owned())
        .spawn(move || {
            let mut coordinator = Coordinator::new(setup, config, commands_rx, snapshot_tx, wake);
            coordinator.startup();
            while coordinator.tick() {}
            let _ = stopped_tx.try_send(());
            coordinator.wake();
        })
        .expect("spawn coordinator thread");
    CoordinatorHandle {
        commands: Some(commands_tx),
        snapshots,
        stopped,
        join: Some(join),
    }
}

// --- Coordinator state ---

/// The recording the coordinator is currently driving.
struct ActiveRecording {
    draft: RecordingDraft,
    mode: RecordingMode,
    started_unix_ms: i64,
    requested_replay_ms: u64,
    /// Wall-clock time the capture stops; set when the activity ended and the
    /// configured overrun is running out.
    stop_at_ms: Option<i64>,
}

/// A capture whose stop was requested and whose artifacts have not arrived.
/// `Finalize` carries the draft the media worker needs; `Discard` only waits
/// so the artifacts can be swept instead of kept.
enum EndingCapture {
    Finalize(Box<RecordingDraft>),
    Discard,
}

/// An activity held back because GSR was still writing the previous capture.
/// `finished` is set when the activity also ended inside that window: the
/// capture still has to start, because the replay buffer holds it, and then
/// stop again after whatever overrun is left.
struct DeferredBegin {
    draft: Box<RecordingDraft>,
    finished: bool,
}

pub struct Coordinator {
    setup: Setup,
    config: Config,
    engine: ActivityEngine,
    recorder: Recorder,
    armed: bool,
    /// Capture is off on purpose because WoW is not running; the replay
    /// buffer starts again with the game.
    standby: bool,
    last_arm_ms: i64,
    next_wow_check: Instant,
    storage: Storage,
    tailers: Vec<LogTailer>,
    /// Per flavour: wall-clock and log time of the newest observed event.
    last_event: HashMap<GameFlavor, (i64, i64)>,
    index: LibraryIndex,
    active: Option<ActiveRecording>,
    /// Set between the stop request and its `CaptureEnded`.
    ending: Option<EndingCapture>,
    /// An activity that began while the previous capture was still flushing.
    deferred_begin: Option<DeferredBegin>,
    /// Injected test end event, released once its wall-clock deadline passes.
    pending_test_end: Option<(i64, ParsedEvent)>,

    media_jobs: SyncSender<MediaJob>,
    media_events_tx: Sender<MediaEvent>,
    media_control: SyncSender<MediaControl>,
    media_events: Receiver<MediaEvent>,
    media_join: Option<JoinHandle<()>>,
    finalize_queue: VecDeque<MediaJob>,
    user_queue: VecDeque<MediaJob>,
    media_busy: Option<WorkKind>,
    work: Option<WorkProgress>,
    /// A failed capture left files in the capture directories while a capture
    /// or media job still owned files there; swept once both are done.
    sweep_pending: bool,

    problems: Vec<Problem>,
    setup_problems: Vec<ValidationProblem>,
    advanced_logging: Vec<(&'static str, Option<bool>)>,
    storage_used_bytes: u64,
    protected_over_limit: bool,

    commands: Receiver<Command>,
    snapshot_tx: SyncSender<Arc<AppSnapshot>>,
    /// Nudges the shell's main loop after a snapshot is queued.
    wake: Box<dyn Fn() + Send>,
    pending_snapshot: Option<Arc<AppSnapshot>>,
    dirty: bool,
    stopping: bool,
}

impl Coordinator {
    pub fn new(
        setup: Setup,
        config: Result<Config, ConfigError>,
        commands: Receiver<Command>,
        snapshot_tx: SyncSender<Arc<AppSnapshot>>,
        wake: Box<dyn Fn() + Send>,
    ) -> Self {
        let (config, mut problems) = match config {
            Ok(config) => (config, Vec::new()),
            Err(ConfigError::NotFound(_)) => (Config::default(), Vec::new()),
            Err(error) => {
                tracing::warn!(%error, "settings could not be loaded; defaults are in use");
                (
                    Config::default(),
                    vec![make_problem(
                        "Settings could not be loaded; defaults are in use.",
                        Some(error.to_string()),
                        Some(RecoveryAction::OpenSettings),
                    )],
                )
            }
        };
        problems.truncate(MAX_PROBLEMS);

        let storage = build_storage(&config);
        let (events_tx, media_events) = mpsc::channel();
        let (media_jobs, media_control, media_join) = spawn_media_worker(
            setup.media.clone(),
            build_storage(&config),
            events_tx.clone(),
        )
        .expect("spawn media worker thread");

        Self {
            setup,
            config,
            engine: ActivityEngine::new(),
            recorder: Recorder::new(),
            armed: false,
            standby: false,
            last_arm_ms: 0,
            next_wow_check: Instant::now(),
            storage,
            tailers: Vec::new(),
            last_event: HashMap::new(),
            index: LibraryIndex::default(),
            active: None,
            ending: None,
            deferred_begin: None,
            pending_test_end: None,
            media_jobs,
            media_events_tx: events_tx,
            media_control,
            media_events,
            media_join: Some(media_join),
            finalize_queue: VecDeque::new(),
            user_queue: VecDeque::new(),
            media_busy: None,
            work: None,
            sweep_pending: false,
            problems,
            setup_problems: Vec::new(),
            advanced_logging: Vec::new(),
            storage_used_bytes: 0,
            protected_over_limit: false,
            commands,
            snapshot_tx,
            wake,
            pending_snapshot: None,
            dirty: true,
            stopping: false,
        }
    }

    /// Validate, scan the library, sweep interrupted artifacts, and arm.
    pub fn startup(&mut self) {
        self.recorder = Recorder::with_timeouts(self.setup.recorder_timeouts);
        self.setup_problems = self.config.validate();
        if !self.setup_problems.is_empty() {
            self.dirty = true;
            return;
        }
        if let Err(error) = self.storage.prepare() {
            self.push_problem(
                "The recording directory could not be prepared.",
                Some(error.to_string()),
                Some(RecoveryAction::OpenSettings),
            );
            self.dirty = true;
            return;
        }
        // One scan serves the library, the limit, and the sweep's references.
        self.rescan();
        let report = self.storage.sweep_orphans(&self.index);
        if !report.failures.is_empty() {
            self.push_problem(
                "Some interrupted recordings could not be moved to Recovery.",
                Some(report.failures.join("; ")),
                Some(RecoveryAction::OpenLogs),
            );
        }
        self.dirty = true;
        // Show the library before arming: spawning gpu-screen-recorder waits
        // out a stability check, and there is no reason for the window to sit
        // empty through it.
        self.publish();

        let tailers_ready = self.open_tailers();
        if self.setup_problems.is_empty() && tailers_ready {
            // Without a stored capture target the portal picker has to show
            // now. Otherwise `check_wow_running` arms once the game runs.
            if self.config.capture.capture_target_token.is_none() {
                self.arm();
            } else {
                self.standby = true;
            }
        }
        self.dirty = true;
        // Make the armed recorder visible before the event loop takes over.
        self.publish();
    }

    /// One iteration of the coordinator loop. Returns `false` once stopped.
    pub fn tick(&mut self) -> bool {
        if self.drain_commands() {
            self.shutdown();
            return false;
        }
        self.poll_recorder();
        self.poll_logs();
        self.poll_media();
        self.check_deadlines();
        self.dispatch_media();
        self.publish();
        true
    }

    /// Blocking-with-timeout command drain; the timeout is the idle pacing.
    /// Returns `true` when shutdown was requested.
    fn drain_commands(&mut self) -> bool {
        let first = match self.commands.recv_timeout(self.setup.poll_interval) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            // The GTK side is gone: stop exactly like an explicit Shutdown.
            Err(RecvTimeoutError::Disconnected) => return true,
        };
        let mut batch: Vec<Command> = first.into_iter().collect();
        while batch.len() < COMMAND_BATCH {
            match self.commands.try_recv() {
                Ok(command) => batch.push(command),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return true,
            }
        }
        for command in batch {
            if command == Command::Shutdown {
                return true;
            }
            self.handle_command(command);
        }
        false
    }

    // --- Commands ---

    fn handle_command(&mut self, command: Command) {
        self.dirty = true;
        match command {
            Command::Arm => {
                self.setup_problems = self.config.validate();
                if self.setup_problems.is_empty() {
                    self.arm();
                }
            }
            Command::Disarm => self.disarm(),
            Command::ForceEnd => self.force_end(),
            Command::StartManual => self.start_manual(),
            Command::StopManual => self.stop_manual(),
            Command::RunTest { category } => self.run_test(&category),
            Command::ReselectCaptureTarget => self.reselect_target(),
            Command::SaveConfig { draft } => self.save_config(*draft),
            Command::SetProtected { ids, value } => {
                self.update_entries(&ids, &EntryUpdate::Protected(value));
            }
            Command::SetTag { id, tag } => {
                self.update_entries(std::slice::from_ref(&id), &EntryUpdate::Tag(tag));
            }
            Command::Delete { ids } => self.delete_entries(&ids),
            Command::CreateClip(range) => self.queue_clip(&range),
            Command::SetSelectedCategory { category } => {
                let mut draft = self.config.clone();
                draft.interface.selected_category = category;
                self.patch_config(draft);
            }
            Command::SetMarkerVisibility {
                deaths,
                encounters,
                rounds,
            } => {
                let mut draft = self.config.clone();
                draft.interface.death_markers = deaths;
                draft.interface.encounter_markers = encounters;
                draft.interface.round_markers = rounds;
                self.patch_config(draft);
            }
            Command::SaveLayout { layout } => {
                if self.config.interface.layout != layout {
                    let mut draft = self.config.clone();
                    draft.interface.layout = layout;
                    self.patch_config(draft);
                }
            }
            Command::DismissReleaseNotes => {
                if self.config.last_seen_version != crate::VERSION {
                    let mut draft = self.config.clone();
                    draft.last_seen_version = crate::VERSION.to_owned();
                    self.patch_config(draft);
                }
            }
            Command::Shutdown => {}
        }
    }

    // --- Recorder lifecycle ---

    fn capture_config(&self) -> CaptureConfig {
        CaptureConfig {
            gsr_binary: self.setup.gsr_binary.clone(),
            data_dir: self.setup.data_dir.clone(),
            ipc_socket: self.setup.ipc_socket.clone(),
            capture_root: capture_root(&self.config),
            settings: self.config.capture.clone(),
        }
    }

    /// True from the moment a capture starts until its artifacts are in hand.
    /// Anything that stops, replaces, or reconfigures gpu-screen-recorder has
    /// to wait: the child is still writing the previous recording.
    fn capture_in_flight(&self) -> bool {
        self.active.is_some() || self.ending.is_some()
    }

    fn arm(&mut self) {
        if self.capture_in_flight() {
            self.push_problem(
                "Screen capture cannot be rearmed while a recording is being captured or saved.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        self.standby = false;
        self.last_arm_ms = now_unix_ms();
        let config = self.capture_config();
        match self.recorder.arm(&config) {
            Ok(()) => {
                self.armed = true;
                self.clear_recovered_capture_problems();
            }
            Err(error) => {
                self.refresh_recorder_health();
                self.push_recorder_problem(&error);
            }
        }
    }

    fn disarm(&mut self) {
        if self.capture_in_flight() {
            self.push_problem(
                "Screen capture cannot be disarmed while a recording is being captured or saved.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        self.armed = false;
        if let Err(error) = self.recorder.shutdown() {
            self.push_recorder_problem(&error);
        }
    }

    /// Run the replay buffer only while WoW is running. A deliberate arm
    /// (first run, Retry, a manual or test recording) counts as activity, so
    /// it is not undone on the next check.
    fn check_wow_running(&mut self, now_ms: i64) {
        if Instant::now() < self.next_wow_check {
            return;
        }
        self.next_wow_check = Instant::now() + self.setup.wow_check_interval;
        let last_write_ms = latest_log_write_ms(&self.config);
        if self.standby {
            if now_ms - last_write_ms < WOW_IDLE_MS {
                tracing::info!("WoW is running: arming");
                self.arm();
                self.dirty = true;
            }
        } else if self.armed
            && !self.capture_in_flight()
            && self.deferred_begin.is_none()
            && self.pending_test_end.is_none()
            && now_ms - last_write_ms.max(self.last_arm_ms) >= WOW_IDLE_MS
        {
            tracing::info!("WoW is not running: capture standby");
            self.disarm();
            self.standby = true;
            self.dirty = true;
        }
    }

    fn reselect_target(&mut self) {
        if self.capture_in_flight() {
            self.push_problem(
                "The capture target cannot change while a recording is being captured or saved.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        let config = self.capture_config();
        match self.recorder.reselect_target(&config) {
            Ok(selection) => {
                self.armed = true;
                self.clear_recovered_capture_problems();
                if let Some(token) = selection.token {
                    self.store_token(token);
                }
            }
            Err(error) => {
                self.refresh_recorder_health();
                self.push_recorder_problem(&error);
            }
        }
    }

    fn store_token(&mut self, token: String) {
        if self.config.capture.capture_target_token.as_deref() == Some(token.as_str()) {
            return;
        }
        self.config.capture.capture_target_token = Some(token);
        if let Err(error) = self.config.save(&self.setup.config_path) {
            self.push_problem(
                "The capture target could not be saved.",
                Some(error.to_string()),
                Some(RecoveryAction::ReselectCaptureTarget),
            );
        }
    }

    fn poll_recorder(&mut self) {
        for event in self.recorder.poll(now_unix_ms()) {
            match event {
                RecorderEvent::TargetTokenAvailable(token) => self.store_token(token),
                RecorderEvent::CaptureEnded { artifacts } => self.capture_ended(artifacts),
                RecorderEvent::RestartFailed { message } => {
                    self.armed = false;
                    self.push_capture_problem(CAPTURE_RESTART_FAILED_PROBLEM, Some(message));
                }
                RecorderEvent::ChildExited { code } => {
                    self.armed = false;
                    self.push_capture_problem(
                        CAPTURE_STOPPED_PROBLEM,
                        Some(format!("gpu-screen-recorder exited with code {code:?}")),
                    );
                    if let Some(active) = self.active.take() {
                        self.pending_test_end = None;
                        self.drop_activity(&active.draft.flavor);
                        self.sweep_capture_dirs();
                    }
                }
                RecorderEvent::Restarted => {
                    self.armed = true;
                    self.clear_recovered_capture_problems();
                    tracing::info!("capture restarted");
                    self.dirty = true;
                }
                RecorderEvent::RestartScheduled { attempt, .. } => {
                    tracing::info!(attempt, "capture restart scheduled");
                }
            }
        }
    }

    // --- Log polling and the activity engine ---

    fn open_tailers(&mut self) -> bool {
        self.tailers.clear();
        self.advanced_logging.clear();
        self.last_event.clear();
        let context = ParseTimeContext::new(self.setup.year, self.setup.media.utc_offset_minutes);
        let mut all_opened = true;
        for (field, flavor, source) in enabled_log_sources(&self.config) {
            self.advanced_logging
                .push((field, advanced_logging_enabled(&source)));
            match LogTailer::open(source.clone(), flavor, context) {
                Ok(tailer) => self.tailers.push(tailer),
                Err(error) => {
                    all_opened = false;
                    self.push_problem(
                        format!("The {field} log folder could not be watched."),
                        Some(error.to_string()),
                        Some(RecoveryAction::OpenSettings),
                    );
                }
            }
        }
        all_opened
    }

    fn poll_logs(&mut self) {
        let mut events = Vec::new();
        for tailer in &mut self.tailers {
            // The tailer logs its own errors, once per distinct message.
            if let Ok(polled) = tailer.poll() {
                events.extend(polled);
            }
        }
        for event in events {
            self.feed(event);
        }
    }

    /// The single entry point for parsed events, shared by live logs and test
    /// recordings.
    fn feed(&mut self, event: ParsedEvent) {
        self.last_event
            .insert(event.flavor.clone(), (now_unix_ms(), event.occurred_at_ms));
        let actions = self.engine.handle(event, &self.config.activities);
        for action in actions {
            self.apply(action);
        }
    }

    fn apply(&mut self, action: ActivityAction) {
        self.dirty = true;
        match action {
            ActivityAction::Begin { draft } => self.begin(*draft, 0),
            ActivityAction::Complete { id, .. } | ActivityAction::Abandon { id, .. } => {
                let Some(draft) = self.engine.take_finished(&id) else {
                    return;
                };
                // The activity both began and ended while the previous capture
                // was flushing. The replay buffer still holds it, so keep the
                // authoritative finished draft and let `capture_ended` start
                // and immediately stop its capture.
                if let Some(deferred) = self.deferred_begin.as_mut()
                    && deferred.draft.id == id
                {
                    *deferred.draft = draft;
                    deferred.finished = true;
                    return;
                }
                let Some(active) = self.active.as_mut() else {
                    return;
                };
                if active.draft.id != id {
                    return;
                }
                let overrun_ms = draft.overrun_ms as i64;
                active.draft = draft;
                active.stop_at_ms = Some(now_unix_ms() + overrun_ms);
            }
            ActivityAction::Discard { id, reason } => {
                let _ = self.engine.take_finished(&id);
                if self
                    .deferred_begin
                    .as_ref()
                    .is_some_and(|deferred| deferred.draft.id == id)
                {
                    // Never captured, nothing written: just forget it.
                    self.deferred_begin = None;
                    tracing::info!(?reason, "discarding deferred recording");
                    return;
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.draft.id == id)
                {
                    tracing::info!(?reason, "discarding recording");
                    self.active = None;
                    self.cancel_capture(&id);
                }
            }
        }
    }

    /// `late_by_ms` is how long after the activity's start its capture begins:
    /// zero live, the flush wait for a deferred activity. The pre-roll covers
    /// it on top of the configured lead-in.
    fn begin(&mut self, draft: RecordingDraft, late_by_ms: i64) {
        // The previous activity already ended (or was just superseded by this
        // one) and is only running out its overrun: cut the overrun short so
        // the new activity takes the deferred path instead of being dropped.
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.stop_at_ms.is_some())
        {
            self.end_capture();
        }
        if self.active.is_some() {
            self.drop_activity(&draft.flavor);
            return;
        }
        // GSR is still writing the previous capture. Hold the draft instead of
        // dropping the activity; the replay buffer keeps filling, so the
        // lead-in is recomputed from the real start when the capture begins.
        if self.ending.is_some() {
            if self.deferred_begin.is_some() {
                self.drop_activity(&draft.flavor);
                return;
            }
            self.deferred_begin = Some(DeferredBegin {
                draft: Box::new(draft),
                finished: false,
            });
            return;
        }
        // Log data arrived before the WoW check noticed the game: record
        // anyway, just without the replay lead-in.
        if self.standby {
            self.arm();
        }
        let capacity_ms = u64::from(self.config.capture.replay_buffer_seconds()) * 1_000;
        let lead_in_ms = i64::from(self.config.capture.extra_lead_in_seconds) * 1_000;
        let requested_replay_ms = (late_by_ms + lead_in_ms).clamp(0, capacity_ms as i64) as u64;
        tracing::info!(
            // How far behind the game the combat log was when it revealed the
            // activity; live captures do not add this to the pre-roll.
            log_delay_ms = now_unix_ms() - draft.started_at_ms,
            late_by_ms,
            requested_replay_ms,
            save_seconds = requested_replay_ms.div_ceil(1_000),
            "starting capture"
        );
        self.start_capture(draft, requested_replay_ms, RecordingMode::Automatic);
    }

    /// Start the capture for a draft, dropping the activity when the recorder
    /// refuses. There is no on-disk pending state to clean up.
    fn start_capture(
        &mut self,
        draft: RecordingDraft,
        requested_replay_ms: u64,
        mode: RecordingMode,
    ) {
        let request = StartRequest {
            id: draft.id.clone(),
            requested_replay_ms,
        };
        match self.recorder.begin(request) {
            Ok(started_unix_ms) => {
                self.active = Some(ActiveRecording {
                    draft,
                    mode,
                    started_unix_ms,
                    requested_replay_ms,
                    stop_at_ms: None,
                });
            }
            Err(error) => {
                self.push_recorder_problem(&error);
                self.drop_activity(&draft.flavor);
            }
        }
    }

    /// Clear the engine's activity for a flavour without recording anything.
    fn drop_activity(&mut self, flavor: &GameFlavor) {
        for action in self.engine.force_end(flavor.clone(), now_unix_ms()) {
            if let ActivityAction::Complete { id, .. }
            | ActivityAction::Abandon { id, .. }
            | ActivityAction::Discard { id, .. } = action
            {
                let _ = self.engine.take_finished(&id);
            }
        }
    }

    fn force_end(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        match active.mode {
            RecordingMode::Manual => {
                self.stop_manual();
                return;
            }
            RecordingMode::Automatic | RecordingMode::Test(_) => {}
        }
        let flavor = active.draft.flavor.clone();
        let occurred_at_ms = now_unix_ms();
        self.pending_test_end = None;
        for action in self.engine.force_end(flavor, occurred_at_ms) {
            self.apply(action);
        }
    }

    /// Retail 10 min, classic/era 2 min without new log data ends at last-data time.
    fn check_data_timeout(&mut self, now_ms: i64) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.mode != RecordingMode::Automatic || active.stop_at_ms.is_some() {
            return;
        }
        let flavor = active.draft.flavor.clone();
        let limit = if flavor == GameFlavor::Retail {
            RETAIL_DATA_TIMEOUT_MS
        } else {
            CLASSIC_DATA_TIMEOUT_MS
        };
        let Some((seen_wall_ms, seen_log_ms)) = self.last_event.get(&flavor).copied() else {
            return;
        };
        let wall_gap_ms = now_ms - seen_wall_ms;
        if wall_gap_ms < limit {
            return;
        }
        tracing::warn!(
            wall_gap_ms,
            seen_log_ms,
            "data timeout: force-ending automatic recording (log idle on disk)"
        );
        for action in self.engine.force_end(flavor, seen_log_ms) {
            self.apply(action);
        }
    }

    // --- Manual and test recordings ---

    fn start_manual(&mut self) {
        if !self.armed && !self.capture_in_flight() {
            self.arm();
        }
        if self.capture_in_flight() || !self.armed || !self.config.manual.enabled {
            self.push_problem(
                "A manual recording could not be started.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        let started_at_ms = now_unix_ms();
        let draft = RecordingDraft {
            id: RecordingId::new(),
            category: Category::Manual,
            flavor: GameFlavor::Retail,
            started_at_ms,
            overrun_ms: 0,
            details: ActivityDetails::Manual,
            player: None,
            combatants: Vec::new(),
            timeline: Vec::new(),
            outcome: None,
            ended_at_ms: None,
            duration_ms: None,
            title: Some("Manual recording".to_owned()),
            activity_hash: None,
            meter: MeterData::default(),
        };
        self.start_capture(draft, 0, RecordingMode::Manual);
    }

    fn stop_manual(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.mode != RecordingMode::Manual {
            return;
        }
        let ended_at_ms = now_unix_ms();
        active.draft.outcome = Some(Outcome::Unknown);
        active.draft.ended_at_ms = Some(ended_at_ms);
        active.draft.duration_ms = Some((ended_at_ms - active.draft.started_at_ms).max(0) as u64);
        active.stop_at_ms = Some(ended_at_ms);
    }

    /// Inject the minimum events for the chosen category, then release the end
    /// event once the test duration has elapsed.
    fn run_test(&mut self, category: &Category) {
        if !self.armed && !self.capture_in_flight() {
            self.arm();
        }
        if self.capture_in_flight() || self.pending_test_end.is_some() || !self.armed {
            self.push_problem(
                "A test recording could not be started.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        let duration = if *category == Category::Raids {
            self.setup.test_duration * 4
        } else {
            self.setup.test_duration
        };
        let start_ms = now_unix_ms();
        let end_ms = start_ms + duration.as_millis() as i64;
        let Some((start_events, end_event)) = test_events(category, start_ms, end_ms) else {
            self.push_problem(
                "That category has no test recording.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        };
        for event in start_events {
            self.feed(event);
        }
        // Nothing was in flight, so any capture now running is this test's.
        if let Some(active) = self.active.as_mut() {
            active.mode = RecordingMode::Test(category.clone());
            self.pending_test_end = Some((end_ms, end_event));
        }
    }

    // --- Ending a capture ---

    fn check_deadlines(&mut self) {
        let now_ms = now_unix_ms();
        if let Some((due_ms, _)) = &self.pending_test_end
            && *due_ms <= now_ms
        {
            let (_, event) = self.pending_test_end.take().expect("checked above");
            self.feed(event);
        }
        self.check_data_timeout(now_ms);
        self.check_wow_running(now_ms);
        if self
            .active
            .as_ref()
            .and_then(|active| active.stop_at_ms)
            .is_some_and(|stop_at_ms| stop_at_ms <= now_ms)
        {
            self.end_capture();
        }
    }

    /// Ask the recorder to stop and remember what to do with the artifacts.
    /// `CaptureEnded` finishes the job; the coordinator keeps serving commands
    /// and snapshots while GSR flushes.
    fn end_capture(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        self.dirty = true;
        match self.recorder.request_end(&active.draft.id) {
            Ok(()) => self.ending = Some(EndingCapture::Finalize(Box::new(active.draft))),
            Err(error) => {
                self.push_recorder_problem(&error);
                self.sweep_capture_dirs();
            }
        }
    }

    /// Stop capture without producing an entry and quarantine what GSR wrote.
    fn cancel_capture(&mut self, id: &RecordingId) {
        self.dirty = true;
        match self.recorder.request_end(id) {
            Ok(()) => self.ending = Some(EndingCapture::Discard),
            Err(error) => self.push_recorder_problem(&error),
        }
    }

    /// The recorder resolved a requested end. Missing artifacts mean GSR never
    /// wrote the regular recording within its bounded wait.
    fn capture_ended(&mut self, artifacts: Option<CaptureArtifacts>) {
        self.dirty = true;
        match (self.ending.take(), artifacts) {
            (Some(EndingCapture::Finalize(draft)), Some(artifacts)) => {
                self.queue_finalization(draft, artifacts);
            }
            (Some(EndingCapture::Finalize(_)), None) => {
                self.push_recorder_problem(&RecorderError::MissingRegularArtifact);
                self.sweep_capture_dirs();
            }
            (Some(EndingCapture::Discard), Some(artifacts)) => {
                let report = self
                    .storage
                    .quarantine_capture(&artifacts, "discarded recording");
                if !report.failures.is_empty() {
                    tracing::warn!(failures = ?report.failures, "discarded capture could not be quarantined");
                }
            }
            (Some(EndingCapture::Discard), None) => self.sweep_capture_dirs(),
            (None, _) => {}
        }
        // Quitting drains this same path and the recorder is killed right
        // after, so a capture started here would be one that nothing ever ends
        // or saves; hold the draft rather than losing it to a shutdown that may
        // still be cancelled by a later tick.
        if self.stopping {
            return;
        }
        let Some(deferred) = self.deferred_begin.take() else {
            return;
        };
        // A disarmed recorder has no child left to start the capture (that is
        // how a mid-flush GSR crash arrives here) and `start_capture` would
        // stack a second, baffling problem on top of the crash the user was
        // already told about. Drop the activity instead: nothing was written.
        if !self.armed {
            tracing::info!("dropping deferred recording: capture is no longer armed");
            return;
        }
        let overrun_ms = deferred.draft.overrun_ms as i64;
        let ended_at_ms = deferred.draft.ended_at_ms;
        // The activity started while the previous capture was flushing; the
        // requested pre-roll still reaches back to the real activity start.
        let late_by_ms = now_unix_ms() - deferred.draft.started_at_ms;
        self.begin(*deferred.draft, late_by_ms);
        // It already ended too. The capture had to start anyway so the replay
        // buffer is written; stop it on the overrun the live path would have
        // used, anchored to when the activity actually ended. A deadline
        // already in the past simply stops on the next tick.
        if deferred.finished
            && let Some(active) = self.active.as_mut()
        {
            let anchor_ms = ended_at_ms.unwrap_or_else(now_unix_ms);
            active.stop_at_ms = Some(anchor_ms + overrun_ms);
        }
    }

    fn queue_finalization(&mut self, draft: Box<RecordingDraft>, artifacts: CaptureArtifacts) {
        if self.finalize_queue.len() >= MAX_MEDIA_QUEUE {
            // Only this capture's files: the queued jobs still need theirs.
            let report = self
                .storage
                .quarantine_capture(&artifacts, "recording that did not fit the save queue");
            if !report.failures.is_empty() {
                tracing::warn!(failures = ?report.failures, "unqueued capture could not be quarantined");
            }
            self.push_problem(
                "The recording could not be queued for saving.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        self.finalize_queue.push_back(MediaJob::FinalizeRecording {
            draft,
            artifacts,
            facts: self.media_facts(),
        });
    }

    fn media_facts(&self) -> MediaFacts {
        MediaFacts {
            fps: Some(self.config.capture.fps),
            width: None,
            height: None,
            codec: Some(self.config.capture.codec),
            has_content: true,
        }
    }

    // --- Media jobs ---

    /// Finalization is always chosen before queued user work, and only one job
    /// is in flight at a time. A capture that has been asked to stop counts as
    /// queued finalization: its artifacts are moments away, and letting a clip
    fn dispatch_media(&mut self) {
        if self.media_busy.is_some() {
            return;
        }
        let finalization_pending = matches!(self.ending, Some(EndingCapture::Finalize(_)));
        let Some(job) = self.finalize_queue.pop_front().or_else(|| {
            if finalization_pending {
                None
            } else {
                self.user_queue.pop_front()
            }
        }) else {
            return;
        };
        let kind = job.kind();
        match self.media_jobs.try_send(job) {
            Ok(()) => {
                self.media_busy = Some(kind);
                self.dirty = true;
            }
            Err(TrySendError::Full(job)) => {
                if kind == WorkKind::Finalize {
                    self.finalize_queue.push_front(job);
                } else {
                    self.user_queue.push_front(job);
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.push_problem(
                    "The media worker is unavailable.",
                    None,
                    Some(RecoveryAction::Quit),
                );
            }
        }
    }

    fn poll_media(&mut self) {
        while let Ok(event) = self.media_events.try_recv() {
            self.dirty = true;
            match event {
                MediaEvent::Progress(progress) => self.work = Some(progress),
                MediaEvent::Completed { entry, .. } => {
                    self.media_busy = None;
                    self.work = None;
                    // The worker already wrote the sidecar: fold its entry
                    // into the index instead of re-parsing the library.
                    self.index.upsert_entry(*entry);
                    self.enforce_limit();
                }
                MediaEvent::Failed { kind, message } => {
                    self.media_busy = None;
                    self.work = None;
                    self.push_problem(
                        match kind {
                            WorkKind::Finalize => "A recording could not be saved.",
                            WorkKind::Clip => "The clip could not be created.",
                        },
                        Some(message),
                        Some(RecoveryAction::OpenLogs),
                    );
                }
                MediaEvent::Cancelled { .. } => {
                    self.media_busy = None;
                    self.work = None;
                }
            }
        }
        if self.sweep_pending {
            self.sweep_capture_dirs();
        }
    }

    /// Quarantine what a failed capture left in the capture directories. A
    /// live capture writes there and queued or running media jobs read and
    /// stage there, so the sweep waits until none is left; `poll_media`
    /// retries every tick.
    fn sweep_capture_dirs(&mut self) {
        if self.capture_in_flight() || self.media_busy.is_some() || !self.finalize_queue.is_empty()
        {
            self.sweep_pending = true;
            return;
        }
        self.sweep_pending = false;
        let report = self.storage.sweep_capture_dirs();
        if !report.failures.is_empty() {
            tracing::warn!(failures = ?report.failures, "capture leftovers could not be quarantined");
        }
        tracing::info!(
            quarantined = report.quarantined.len(),
            "capture leftovers swept"
        );
    }

    fn queue_clip(&mut self, range: &ClipRange) {
        let Some(source) = self.entry(&range.source).cloned() else {
            self.push_problem(
                "The clip source is no longer in the library.",
                Some(range.source.to_string()),
                Some(RecoveryAction::Retry),
            );
            return;
        };
        if self.user_queue.len() >= MAX_MEDIA_QUEUE {
            self.push_problem(
                "The media work queue is full.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        self.user_queue.push_back(MediaJob::CreateClip {
            source: Box::new(source),
            start_ms: range.start_ms,
            end_ms: range.end_ms,
        });
    }

    // --- Library mutations ---

    fn entry(&self, id: &RecordingId) -> Option<&LibraryEntry> {
        self.index.entries.iter().find(|entry| &entry.id == id)
    }

    fn update_entries(&mut self, ids: &[RecordingId], change: &EntryUpdate) {
        for id in ids {
            // Servicing between writes keeps a bulk pass from delaying log
            // handling and recorder deadlines by the full batch. Commands are
            // not drained (ordering is preserved) and media completions are
            // not polled: their fold enforces the storage limit, which could
            // evict entries this batch has not protected yet.
            self.poll_recorder();
            self.poll_logs();
            self.check_deadlines();
            let Some(entry) = self.entry(id).cloned() else {
                self.push_problem(
                    "A selected recording is no longer in the library.",
                    Some(id.to_string()),
                    Some(RecoveryAction::Retry),
                );
                continue;
            };
            match self.storage.update(&entry, change) {
                Ok(updated) => {
                    if let Some(slot) = Arc::make_mut(&mut self.index.entries)
                        .iter_mut()
                        .find(|slot| &slot.id == id)
                    {
                        *slot = updated;
                    }
                }
                Err(error) => self.push_problem(
                    format!("\"{}\" could not be updated.", entry.title),
                    Some(error.to_string()),
                    Some(RecoveryAction::Retry),
                ),
            }
        }
        // Correlation keys on activity hash, start and category; a tag or
        // protect flag cannot move an entry between groups, so there is
        // nothing to recompute.
        self.enforce_limit();
    }

    fn delete_entries(&mut self, ids: &[RecordingId]) {
        let mut entries = Vec::new();
        for id in ids {
            match self.entry(id).cloned() {
                Some(entry) => entries.push(entry),
                None => self.push_problem(
                    "A selected recording is no longer in the library.",
                    Some(id.to_string()),
                    Some(RecoveryAction::Retry),
                ),
            }
        }
        let result = self.storage.delete(&entries);
        for (id, error) in &result.failures {
            let title = self
                .entry(id)
                .map_or_else(|| id.to_string(), |entry| entry.title.clone());
            self.push_problem(
                format!("\"{title}\" could not be deleted."),
                Some(error.clone()),
                Some(RecoveryAction::Retry),
            );
        }
        // Fold the deletions into the index the way completion folds in a new
        // entry: correlations rebuild over what remains, so a surviving
        // viewpoint becomes its own group exactly as a rescan would produce.
        // Only a sidecar that outlived its media needs the directory itself.
        if result.partially_deleted {
            self.rescan();
        } else {
            self.index.remove_entries(&result.deleted);
            self.enforce_limit();
        }
    }

    fn rescan(&mut self) {
        self.index = self.storage.scan();
        for skipped in &self.index.skipped {
            tracing::warn!(
                sidecar = %skipped.sidecar_path.display(),
                reason = %skipped.reason,
                "recording skipped"
            );
        }
        self.enforce_limit();
    }

    /// Evict down to the limit and recount the library's size; storage
    /// treats Unlimited as count-only.
    fn enforce_limit(&mut self) {
        let result = self
            .storage
            .enforce_limit(self.config.storage.limit, &self.index.entries);
        self.protected_over_limit = result.protected_over_limit;
        if result.partially_deleted {
            // A sidecar outlived its media; only a rescan can reconcile. The
            // rescan skips that sidecar, so this does not repeat.
            self.rescan();
            return;
        }
        self.index.remove_entries(&result.evicted);
        self.storage_used_bytes = result.remaining_bytes;
        self.dirty = true;
    }

    // --- Configuration ---

    fn restart_media_worker(&mut self) -> Result<(), String> {
        let _ = self.media_control.send(MediaControl::Shutdown {
            pending_finalizations: Vec::new(),
        });
        if let Some(join) = self.media_join.take() {
            join.join()
                .map_err(|_| "the previous media worker panicked".to_owned())?;
        }
        let (media_jobs, media_control, media_join) = spawn_media_worker(
            self.setup.media.clone(),
            build_storage(&self.config),
            self.media_events_tx.clone(),
        )?;
        self.media_jobs = media_jobs;
        self.media_control = media_control;
        self.media_join = Some(media_join);
        Ok(())
    }

    /// UI-only fields: adopt, show, then save atomically; reconfigure nothing.
    ///
    /// Persisting first would put two `fsync`s in front of every category
    /// click. Nothing rereads the file at runtime, so the snapshot can go out
    /// first; a failed write rolls the preference back.
    fn patch_config(&mut self, draft: Config) {
        let previous = std::mem::replace(&mut self.config, draft);
        self.dirty = true;
        self.publish();
        if let Err(error) = self.config.save(&self.setup.config_path) {
            // The rename may already have made the new file visible; only the
            // directory sync failed. Rolling back then would leave the next
            // launch loading a value this one just told the user it dropped.
            if !error.is_committed() {
                self.config = previous;
            }
            self.push_problem(
                "Settings could not be saved.",
                Some(error.to_string()),
                Some(RecoveryAction::Retry),
            );
        }
    }

    fn save_config(&mut self, mut draft: Config) {
        if self.capture_in_flight()
            || self.media_busy.is_some()
            || !self.finalize_queue.is_empty()
            || !self.user_queue.is_empty()
        {
            self.push_problem(
                "Settings cannot change while a recording is being captured or saved.",
                None,
                Some(RecoveryAction::Retry),
            );
            return;
        }
        // Settings never owns the release-notes acknowledgement. Its draft was
        // cloned from a snapshot taken while the "What's new" dialog was still
        // up -- its buttons open Settings -- so honouring the draft here
        // replays the notes on every save.
        draft.last_seen_version = self.config.last_seen_version.clone();
        let problems = draft.validate();
        if !problems.is_empty() {
            self.setup_problems = problems;
            self.push_problem(
                "Those settings are not usable yet.",
                None,
                Some(RecoveryAction::OpenSettings),
            );
            return;
        }

        let logs_changed = draft.flavors != self.config.flavors
            || draft.validate_log_paths != self.config.validate_log_paths;
        let storage_changed = draft.storage.recording_dir != self.config.storage.recording_dir;
        let capture_changed = draft.capture != self.config.capture;
        let limit_changed = draft.storage.limit != self.config.storage.limit;

        if let Err(error) = draft.save(&self.setup.config_path) {
            self.push_problem(
                "Settings could not be saved.",
                Some(error.to_string()),
                Some(RecoveryAction::Retry),
            );
            return;
        }
        self.config = draft;
        self.setup_problems = Vec::new();

        let mut runtime_ready = true;
        if logs_changed && !self.open_tailers() {
            runtime_ready = false;
        }
        if storage_changed {
            self.storage = build_storage(&self.config);
            if let Err(error) = self.storage.prepare() {
                runtime_ready = false;
                self.push_problem(
                    "The new recording directory could not be prepared.",
                    Some(error.to_string()),
                    Some(RecoveryAction::OpenSettings),
                );
            }
            self.rescan();
            if let Err(error) = self.restart_media_worker() {
                runtime_ready = false;
                self.push_problem(
                    "The media worker could not switch to the new storage directory.",
                    Some(error),
                    Some(RecoveryAction::Retry),
                );
            }
        }
        // In standby the new settings simply apply on the next arm.
        if runtime_ready && (capture_changed || storage_changed) && !self.standby {
            self.arm();
        }
        if !runtime_ready {
            self.disarm();
        } else if limit_changed || storage_changed {
            self.enforce_limit();
        }
        if (capture_changed || storage_changed) && !self.armed && !self.standby {
            // The saved config stands; only the runtime is down.
            self.push_problem(
                "Screen capture did not restart with the new settings.",
                None,
                Some(RecoveryAction::Retry),
            );
        }
    }

    // --- Snapshot and problems ---

    fn push_problem(
        &mut self,
        summary: impl Into<String>,
        safe_detail: Option<String>,
        recovery_action: Option<RecoveryAction>,
    ) {
        let problem = make_problem(summary, safe_detail, recovery_action);
        tracing::warn!(summary = %problem.summary, detail = ?problem.safe_detail, "problem");
        self.problems.push(problem);
        if self.problems.len() > MAX_PROBLEMS {
            self.problems.remove(0);
        }
        self.dirty = true;
    }

    fn clear_recovered_capture_problems(&mut self) {
        if clear_recovered_capture_problems(&mut self.problems) {
            self.dirty = true;
        }
    }

    fn refresh_recorder_health(&mut self) {
        self.armed = self.recorder.is_running();
        if self.armed {
            self.clear_recovered_capture_problems();
        }
    }

    fn push_capture_problem(&mut self, summary: &'static str, safe_detail: Option<String>) {
        self.problems
            .retain(|problem| problem.summary.as_str() != summary);
        self.push_problem(
            summary,
            safe_detail,
            Some(RecoveryAction::ReselectCaptureTarget),
        );
    }

    fn push_recorder_problem(&mut self, error: &RecorderError) {
        let (summary, detail, action) = match error {
            RecorderError::SelectionDenied { log_tail } => (
                "Screen capture was not allowed.",
                Some(log_tail.clone()),
                RecoveryAction::ReselectCaptureTarget,
            ),
            RecorderError::SpawnFailed { message, log_tail } => (
                "Screen capture could not start.",
                Some(format!("{message}\n{log_tail}")),
                RecoveryAction::OpenLogs,
            ),
            RecorderError::MissingRegularArtifact => (
                "The recording produced no video file.",
                None,
                RecoveryAction::OpenLogs,
            ),
            RecorderError::InvalidSettings(message) => (
                "The capture settings are not usable.",
                Some(message.clone()),
                RecoveryAction::OpenSettings,
            ),
            RecorderError::Busy | RecorderError::NotArmed | RecorderError::WrongId => (
                "The recorder was not ready for that.",
                Some(format!("{error:?}")),
                RecoveryAction::Retry,
            ),
            RecorderError::Io(error) => (
                "Screen capture failed.",
                Some(error.to_string()),
                RecoveryAction::OpenLogs,
            ),
        };
        self.push_problem(summary, detail, Some(action));
    }

    fn status(&self) -> RecorderStatus {
        if !self.setup_problems.is_empty() {
            return RecorderStatus::SetupRequired;
        }
        if let Some(active) = &self.active {
            let title = active
                .draft
                .title
                .clone()
                .unwrap_or_else(|| format!("{:?}", active.draft.category));
            return match active.stop_at_ms {
                Some(_) => RecorderStatus::Overrunning {
                    title,
                    started_unix_ms: active.started_unix_ms,
                },
                None => RecorderStatus::Recording {
                    category: active.draft.category.clone(),
                    title,
                    started_unix_ms: active.started_unix_ms,
                    manual: active.mode == RecordingMode::Manual,
                    test: matches!(active.mode, RecordingMode::Test(_)),
                },
            };
        }
        // A stop was requested and GSR is still writing: the recording is not
        // over from the user's side, so do not fall back to Ready. A discarded
        // capture is not being saved, so say what is actually happening.
        if matches!(self.ending, Some(EndingCapture::Discard)) {
            return RecorderStatus::Finalizing {
                title: "Discarding recording".to_owned(),
            };
        }
        if self.ending.is_some() || self.media_busy == Some(WorkKind::Finalize) {
            return RecorderStatus::Finalizing {
                title: "Saving recording".to_owned(),
            };
        }
        if !self.armed {
            return RecorderStatus::WaitingForWow;
        }
        RecorderStatus::Ready
    }

    fn build_snapshot(&self) -> Arc<AppSnapshot> {
        let active = self.active.as_ref().map(|active| ActiveRecordingView {
            id: active.draft.id.clone(),
            category: active.draft.category.clone(),
            title: active
                .draft
                .title
                .clone()
                .unwrap_or_else(|| format!("{:?}", active.draft.category)),
            mode: active.mode.clone(),
            started_unix_ms: active.started_unix_ms,
            requested_replay_ms: active.requested_replay_ms,
        });
        Arc::new(AppSnapshot {
            entries: Arc::clone(&self.index.entries),
            correlations: Arc::clone(&self.index.correlations),
            category_counts: category_counts(&self.index.entries),
            status: self.status(),
            active,
            config: self.config.clone(),
            setup_problems: self.setup_problems.clone(),
            advanced_logging: self.advanced_logging.clone(),
            problems: self.problems.clone(),
            work: self.work.clone(),
            queued_jobs: self.finalize_queue.len() + self.user_queue.len(),
            storage_used_bytes: self.storage_used_bytes,
            protected_over_limit: self.protected_over_limit,
        })
    }

    /// Publish the newest state, keeping at most one unsent snapshot locally.
    fn publish(&mut self) {
        if !self.dirty && self.pending_snapshot.is_none() {
            return;
        }
        let snapshot = if self.dirty {
            self.dirty = false;
            self.pending_snapshot = None;
            self.build_snapshot()
        } else {
            self.pending_snapshot.take().expect("checked above")
        };
        match self.snapshot_tx.try_send(snapshot) {
            Ok(()) => self.wake(),
            Err(TrySendError::Full(unsent)) => self.pending_snapshot = Some(unsent),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Nudge the shell's main loop. Cheap and nonblocking by contract.
    pub fn wake(&self) {
        (self.wake)();
    }

    // --- Shutdown ---

    fn shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        // Resolve the active capture the same way a force end would.
        if self.active.is_some() {
            self.force_end();
            if let Some(active) = self.active.as_mut() {
                active.stop_at_ms = Some(now_unix_ms());
            }
            self.check_deadlines();
        }
        // Quitting is the one place that must wait: `recorder.shutdown` kills
        // GSR, so the requested end has to produce its artifacts first or the
        // recording is lost. Same bounded deadlines the poll path uses.
        if self.recorder.is_ending() {
            for event in self
                .recorder
                .finish_end_blocking(Instant::now() + QUIT_END_GRACE)
            {
                if let RecorderEvent::CaptureEnded { artifacts } = event {
                    self.capture_ended(artifacts);
                }
            }
        }
        if let Err(error) = self.recorder.shutdown() {
            tracing::warn!(?error, "recorder shutdown failed");
        }
        self.armed = false;
        // Finalization gets the media worker's grace period; user jobs cancel.
        self.user_queue.clear();
        // Transfer every not-yet-submitted finalization in the shutdown
        // message. This cannot race with the capacity-one job channel: a job
        // sent from `dispatch_media` is still in flight, not in the queue.
        let pending_finalizations = self.finalize_queue.drain(..).collect();
        let _ = self.media_control.send(MediaControl::Shutdown {
            pending_finalizations,
        });
        if let Some(join) = self.media_join.take() {
            let _ = join.join();
        }
        // GSR and the media worker are gone, so nothing owns the capture
        // directories; the storage folder is left to the next startup sweep.
        let report = self.storage.sweep_capture_dirs();
        if !report.failures.is_empty() {
            tracing::warn!(failures = ?report.failures, "shutdown sweep failed");
        }
        self.finalize_queue.clear();
        self.poll_media();
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// --- Free helpers ---

fn clear_recovered_capture_problems(problems: &mut Vec<Problem>) -> bool {
    let previous_len = problems.len();
    problems.retain(|problem| {
        !matches!(
            problem.summary.as_str(),
            CAPTURE_STOPPED_PROBLEM | CAPTURE_RESTART_FAILED_PROBLEM
        )
    });
    problems.len() != previous_len
}

fn make_problem(
    summary: impl Into<String>,
    safe_detail: Option<String>,
    recovery_action: Option<RecoveryAction>,
) -> Problem {
    Problem {
        summary: summary.into(),
        safe_detail,
        occurred_unix_ms: now_unix_ms(),
        recovery_action,
    }
}

/// GSR's `replay`/`regular`/`staging` directories live under the library root.
fn capture_root(config: &Config) -> PathBuf {
    config.storage.recording_dir.path.clone()
}

fn build_storage(config: &Config) -> Storage {
    Storage::new(
        config.storage.recording_dir.path.clone(),
        capture_root(config),
    )
}

type MediaWorkerHandles = (
    SyncSender<MediaJob>,
    SyncSender<MediaControl>,
    JoinHandle<()>,
);

fn spawn_media_worker(
    config: MediaConfig,
    storage: Storage,
    events: Sender<MediaEvent>,
) -> Result<MediaWorkerHandles, String> {
    let (jobs, jobs_rx) = mpsc::sync_channel(1);
    let (control, control_rx) = mpsc::sync_channel(1);
    let worker = MediaWorker::new(config, storage, jobs_rx, control_rx, events);
    let join = std::thread::Builder::new()
        .name("media".to_owned())
        .spawn(move || worker.run())
        .map_err(|error| format!("spawn media worker: {error}"))?;
    Ok((jobs, control, join))
}

fn enabled_log_sources(config: &Config) -> Vec<(&'static str, GameFlavor, PathBuf)> {
    [
        ("retail", GameFlavor::Retail, &config.flavors.retail),
        ("retail_ptr", GameFlavor::Retail, &config.flavors.retail_ptr),
        ("classic", GameFlavor::Classic, &config.flavors.classic),
        (
            "classic_ptr",
            GameFlavor::Classic,
            &config.flavors.classic_ptr,
        ),
        ("era", GameFlavor::Era, &config.flavors.era),
    ]
    .into_iter()
    .filter(|(_, _, flavor)| flavor.enabled && !flavor.log_dir.path.as_os_str().is_empty())
    .map(|(field, game, flavor)| (field, game, flavor.log_dir.path.clone()))
    .collect()
}

/// Newest modification time, in Unix milliseconds, of anything in the enabled
/// Logs folders. WoW writes its client logs there from launch to exit, unlike
/// the combat log, which only exists while combat logging is on.
fn latest_log_write_ms(config: &Config) -> i64 {
    enabled_log_sources(config)
        .iter()
        .filter_map(|(_, _, source)| {
            let dir = if source.is_file() {
                source.parent()?
            } else {
                source
            };
            std::fs::read_dir(dir).ok()
        })
        .flatten()
        // The file's own time, not the listing's: Windows' directory listing
        // lags for files the game holds open, such as the live combat log.
        .filter_map(|entry| std::fs::metadata(entry.ok()?.path()).ok()?.modified().ok())
        .filter_map(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .max()
        .unwrap_or(0)
}

/// Whether the `Config.wtf` beside the Logs folder carries
/// `SET advancedCombatLogging "1"`. `None` means the file could not be read
/// rather than that the setting is off: the folder portal exports the chosen
/// Logs directory on its own, so the `WTF` sibling is outside the sandbox and
/// a failed read says nothing about the game's configuration.
fn advanced_logging_enabled(log_dir: &Path) -> Option<bool> {
    let parent = log_dir.parent()?;
    let text = std::fs::read_to_string(parent.join("WTF").join("Config.wtf")).ok()?;
    Some(text.lines().any(|line| {
        let line = line.trim();
        line.starts_with("SET advancedCombatLogging") && line.rsplit(' ').next() == Some("\"1\"")
    }))
}

fn category_counts(entries: &[LibraryEntry]) -> Vec<(Category, usize)> {
    let mut counts: Vec<(Category, usize)> = Vec::new();
    for entry in entries {
        match counts
            .iter_mut()
            .find(|(category, _)| category == &entry.category)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((entry.category.clone(), 1)),
        }
    }
    counts
}

#[cfg(windows)]
fn local_clock() -> (i32, i32) {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: both calls only fill the out-structs they are given.
    let (local, zone, which) = unsafe {
        let mut local = std::mem::zeroed();
        GetLocalTime(&mut local);
        let mut zone: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let which = GetTimeZoneInformation(&mut zone);
        (local, zone, which)
    };
    // Bias is minutes to add to local time to get UTC; 2 means DST is active.
    let bias = zone.Bias
        + if which == 2 {
            zone.DaylightBias
        } else {
            zone.StandardBias
        };
    (i32::from(local.wYear), -bias)
}

#[cfg(unix)]
fn local_clock() -> (i32, i32) {
    let seconds = now_unix_ms().div_euclid(1_000) as libc::time_t;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    // `localtime_r` follows the system's configured zone, including DST.
    // Should it fail, UTC (offset zero) stands in.
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        unsafe { libc::gmtime_r(&seconds, &mut local) };
    }
    (local.tm_year + 1900, (local.tm_gmtoff / 60) as i32)
}

/// The minimum parsed events that drive one category through the activity
/// engine, used by test recordings.
fn test_events(
    category: &Category,
    start_ms: i64,
    end_ms: i64,
) -> Option<(Vec<ParsedEvent>, ParsedEvent)> {
    const GUID: &str = "Player-1092-0A70E103";
    const NAME: &str = "Testplayer-Testrealm";
    // Affiliation mine, friendly, player-controlled, player type.
    const SELF_FLAGS: u64 = 0x511;
    const ALLY_GUID: &str = "Player-1092-0B80F204";
    const ALLY_NAME: &str = "Testmage-Testrealm";
    const ALLY_FLAGS: u64 = 0x512;
    const HOSTILE_FLAGS: u64 = 0xa48;
    const ENEMY_GUID: &str = "Creature-0-TEST";
    const ENEMY_NAME: &str = "Test Target";

    let retail = |event: CombatEvent, at_ms: i64| ParsedEvent {
        flavor: GameFlavor::Retail,
        occurred_at_ms: at_ms,
        event,
    };
    let arena = |zone_id: u32, match_type: &str| CombatEvent::ArenaStarted {
        zone_id,
        match_type: match_type.to_owned(),
    };

    let (start, end) = match category {
        Category::TwoVTwo => (
            arena(2547, "2v2"),
            CombatEvent::ArenaEnded { winning_team_id: 0 },
        ),
        Category::ThreeVThree => (
            arena(980, "3v3"),
            CombatEvent::ArenaEnded { winning_team_id: 0 },
        ),
        Category::SoloShuffle => (
            arena(2547, "Rated Solo Shuffle"),
            CombatEvent::ArenaEnded { winning_team_id: 0 },
        ),
        Category::Raids => (
            CombatEvent::EncounterStarted {
                encounter_id: 2820,
                name: "Test Encounter".to_owned(),
                difficulty_id: 16,
            },
            CombatEvent::EncounterEnded {
                difficulty_id: 16,
                success: true,
            },
        ),
        Category::Battlegrounds => (
            CombatEvent::ZoneChanged { zone_id: 30 },
            CombatEvent::ZoneChanged { zone_id: 0 },
        ),
        Category::MythicPlus => (
            CombatEvent::ChallengeStarted {
                zone_id: 2286,
                map_id: 377,
                level: 10,
                affixes: vec![9, 6, 3],
            },
            CombatEvent::ChallengeEnded {
                success: true,
                duration_ms: (end_ms - start_ms).max(0) as u64,
            },
        ),
        _ => return None,
    };

    let mut start_events = vec![
        retail(start, start_ms),
        retail(
            CombatEvent::Combatant {
                guid: GUID.to_owned(),
                team_id: Some(0),
                spec_id: Some(577),
            },
            start_ms,
        ),
        retail(
            CombatEvent::Combatant {
                guid: ALLY_GUID.to_owned(),
                team_id: Some(0),
                spec_id: Some(63),
            },
            start_ms,
        ),
        retail(
            CombatEvent::PlayerObserved {
                kind: PlayerObservationKind::AuraApplied,
                aura_type: None,
                spell_id: 0,
                guid: GUID.to_owned(),
                name: NAME.to_owned(),
                flags: SELF_FLAGS,
                target_guid: GUID.to_owned(),
                target_name: NAME.to_owned(),
                target_flags: SELF_FLAGS,
                spell_name: "Test Recording".to_owned(),
                owner_guid: None,
            },
            start_ms,
        ),
    ];
    let duration_ms = (end_ms - start_ms).max(0);
    for (quarter, player_amount, ally_amount) in [
        (1, 1_000_000, 800_000),
        (2, 1_500_000, 1_100_000),
        (3, 2_000_000, 1_400_000),
    ] {
        let at_ms = start_ms + duration_ms * quarter / 4;
        for (guid, name, flags, spell, amount) in [
            (GUID, NAME, SELF_FLAGS, "Annihilation", player_amount),
            (ALLY_GUID, ALLY_NAME, ALLY_FLAGS, "Pyroblast", ally_amount),
        ] {
            start_events.push(retail(
                CombatEvent::Damage {
                    source_guid: guid.to_owned(),
                    source_name: name.to_owned(),
                    source_flags: flags,
                    source_owner_guid: None,
                    dest_guid: "Creature-0-TEST".to_owned(),
                    dest_name: "Test Target".to_owned(),
                    dest_flags: HOSTILE_FLAGS,
                    dest_raid_marker: 0,
                    spell_name: spell.to_owned(),
                    amount,
                    dest_current_hp: None,
                    dest_max_hp: None,
                },
                at_ms,
            ));
        }
        start_events.push(retail(
            CombatEvent::Damage {
                source_guid: ENEMY_GUID.to_owned(),
                source_name: ENEMY_NAME.to_owned(),
                source_flags: HOSTILE_FLAGS,
                source_owner_guid: None,
                dest_guid: ALLY_GUID.to_owned(),
                dest_name: ALLY_NAME.to_owned(),
                dest_flags: ALLY_FLAGS,
                dest_raid_marker: 0,
                spell_name: "Void Strike".to_owned(),
                amount: 180_000 * quarter as u64,
                dest_current_hp: None,
                dest_max_hp: None,
            },
            at_ms,
        ));
        start_events.push(retail(
            CombatEvent::Heal {
                source_guid: GUID.to_owned(),
                source_name: NAME.to_owned(),
                source_flags: SELF_FLAGS,
                dest_guid: ALLY_GUID.to_owned(),
                dest_name: ALLY_NAME.to_owned(),
                dest_flags: ALLY_FLAGS,
                dest_raid_marker: 0,
                spell_name: "Soul Mend".to_owned(),
                dest_current_hp: None,
                dest_max_hp: None,
                amount: 120_000 * quarter as u64,
                overheal: 0,
            },
            at_ms + 1,
        ));
    }
    start_events.push(retail(
        CombatEvent::UnitDied {
            guid: ALLY_GUID.to_owned(),
            name: ALLY_NAME.to_owned(),
            flags: ALLY_FLAGS,
            unconscious: false,
        },
        end_ms - 1,
    ));
    Some((start_events, retail(end, end_ms)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        ActiveRecording, CaptureArtifacts, Coordinator, EndingCapture, EntryUpdate, MediaConfig,
        RecordingDraft, RecordingMode, Setup, Storage, Timeouts, now_unix_ms,
    };
    use crate::config::{CONFIG_FILENAME, Config};
    use crate::domain::{
        ActivityDetails, Category, GameFlavor, LibraryEntry, MediaFacts, MeterData, Outcome,
        RecordingId, WorkKind,
    };
    use crate::storage::{RECOVERY_DIR, test_root};

    /// A failed or discarded capture must not take a queued finalization's
    /// inputs with it: a discard quarantines only its own files, and a
    /// capture-directory sweep waits until the media worker is idle.
    #[test]
    fn failed_capture_sweeps_leave_busy_media_work_alone() {
        let root = test_root("capture-sweep");
        let library = root.join("library");
        let regular_dir = root.join("buffer/regular");
        std::fs::create_dir_all(&regular_dir).unwrap();
        let queued = regular_dir.join("Video_queued.mkv");
        std::fs::write(&queued, b"queued").unwrap();
        let discarded = regular_dir.join("Video_discarded.mkv");
        std::fs::write(&discarded, b"discarded").unwrap();

        {
            let (_commands, commands_rx) = mpsc::sync_channel(1);
            let (snapshot_tx, _snapshots) = mpsc::sync_channel(1);
            let mut coordinator = Coordinator::new(
                Setup {
                    config_path: root.join(CONFIG_FILENAME),
                    data_dir: root.join("recorder"),
                    ipc_socket: root.join("recorder/gsr.sock"),
                    gsr_binary: PathBuf::from("true"),
                    media: MediaConfig::default(),
                    year: 2026,
                    recorder_timeouts: Timeouts::default(),
                    wow_check_interval: Duration::from_secs(5),
                    poll_interval: Duration::from_millis(5),
                    test_duration: Duration::from_millis(200),
                },
                Ok(Config::default()),
                commands_rx,
                snapshot_tx,
                Box::new(|| {}),
            );
            coordinator.storage = Storage::new(&library, root.join("buffer"));
            // The queued recording's finalization is running.
            coordinator.media_busy = Some(WorkKind::Finalize);

            coordinator.ending = Some(EndingCapture::Discard);
            coordinator.capture_ended(Some(CaptureArtifacts {
                replay: None,
                regular: discarded.clone(),
                regular_started_at_ms: 0,
                regular_stopped_at_ms: 0,
            }));
            assert!(!discarded.exists(), "the discarded capture was kept");
            assert!(queued.exists(), "the discard swept a queued input");

            // GSR never produced the regular file: sweep, but not yet.
            coordinator.ending = Some(EndingCapture::Discard);
            coordinator.capture_ended(None);
            assert!(queued.exists(), "the sweep ran while media work was busy");

            coordinator.media_busy = None;
            coordinator.poll_media();
            assert!(!queued.exists(), "the deferred sweep never ran");
            assert_eq!(
                std::fs::read_dir(library.join(RECOVERY_DIR))
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mkv"))
                    .count(),
                2
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn bulk_mutation_services_a_due_capture_deadline_mid_batch() {
        let root = test_root("midbatch");
        let library = root.join("library");
        std::fs::create_dir_all(&library).unwrap();
        let sidecar_storage = Storage::new(&library, root.join("buffer"));
        let mut entries = Vec::new();
        for name in ["bulk-a", "bulk-b"] {
            let (media_path, sidecar_path) = sidecar_storage.claim_output(name).unwrap();
            let source = library.join(format!("{name}-source.mp4"));
            std::fs::write(&source, b"media").unwrap();
            let entry = LibraryEntry {
                id: RecordingId::new(),
                media_path,
                sidecar_path,
                category: Category::Manual,
                flavor: GameFlavor::Retail,
                title: name.to_owned(),
                start_unix_ms: now_unix_ms() - 61_000,
                duration_ms: 60_000,
                outcome: Outcome::Unknown,
                protected: false,
                tag: None,
                activity_hash: None,
                player: None,
                combatants: Vec::new(),
                details: ActivityDetails::Manual,
                timeline: Vec::new(),
                media: MediaFacts {
                    fps: None,
                    width: None,
                    height: None,
                    codec: None,
                    has_content: true,
                },
            };
            sidecar_storage
                .write_new_entry(&entry, &MeterData::default(), &source)
                .unwrap();
            entries.push(entry);
        }

        {
            let (_commands, commands_rx) = mpsc::sync_channel(1);
            let (snapshot_tx, _snapshots) = mpsc::sync_channel(1);
            let mut coordinator = Coordinator::new(
                Setup {
                    config_path: root.join(CONFIG_FILENAME),
                    data_dir: root.join("recorder"),
                    ipc_socket: root.join("recorder/gsr.sock"),
                    gsr_binary: PathBuf::from("true"),
                    media: MediaConfig::default(),
                    year: 2026,
                    recorder_timeouts: Timeouts::default(),
                    wow_check_interval: Duration::from_secs(5),
                    poll_interval: Duration::from_millis(5),
                    test_duration: Duration::from_millis(200),
                },
                Ok(Config::default()),
                commands_rx,
                snapshot_tx,
                Box::new(|| {}),
            );
            // Never let the default-config storage point at real directories.
            coordinator.storage = Storage::new(&library, root.join("buffer"));
            coordinator.index.entries = Arc::new(entries.clone());
            coordinator.active = Some(ActiveRecording {
                draft: RecordingDraft {
                    id: RecordingId::new(),
                    category: Category::Raids,
                    flavor: GameFlavor::Retail,
                    started_at_ms: now_unix_ms(),
                    overrun_ms: 0,
                    details: ActivityDetails::Manual,
                    player: None,
                    combatants: Vec::new(),
                    timeline: Vec::new(),
                    outcome: None,
                    ended_at_ms: None,
                    duration_ms: None,
                    title: None,
                    activity_hash: None,
                    meter: MeterData::default(),
                },
                mode: RecordingMode::Automatic,
                started_unix_ms: now_unix_ms(),
                requested_replay_ms: 0,
                stop_at_ms: Some(now_unix_ms()),
            });

            let mut ids: Vec<RecordingId> = entries.iter().map(|entry| entry.id.clone()).collect();
            // The lookup failure stays in the batch so its problem pins the
            // servicing order.
            ids.push(RecordingId::new());
            coordinator.update_entries(&ids, &EntryUpdate::Protected(true));

            assert!(
                coordinator.active.is_none(),
                "the due capture deadline was not serviced during the batch"
            );
            assert!(
                coordinator
                    .index
                    .entries
                    .iter()
                    .all(|entry| entry.protected),
                "the protection writes did not land in the index"
            );
            let summaries: Vec<&str> = coordinator
                .problems
                .iter()
                .map(|problem| problem.summary.as_str())
                .collect();
            assert_eq!(
                summaries,
                vec![
                    "The recorder was not ready for that.",
                    "A selected recording is no longer in the library.",
                ]
            );
        } // The coordinator joins its media worker here, before temp cleanup.
        std::fs::remove_dir_all(&root).ok();
    }
}
