// SPDX-License-Identifier: GPL-3.0-or-later

//! gpu-screen-recorder lifecycle adapter.
//!
//! One long-lived GSR replay-buffer child, at most one active recording, and
//! GSR's `-ipc` socket: one JSON request per line (`save-replay` sized to the
//! pre-roll, `start-replay-recording`, `stop-replay-recording`), answered by
//! replies that carry the saved paths. `poll` drains them without blocking.
//!
//! - Restart delays are 2, 4, 8, 16, then capped 30 seconds indefinitely. The
//!   attempt counter resets on a deliberate `arm`, or when the child that
//!   exited had stayed up for `STABLE_CHILD`: a crash loop keeps backing off,
//!   an occasional exit during a long session starts over at 2 seconds.
//! - Crash recovery of interrupted recordings is not Recorder's job; it keeps
//!   no persistent state.
//!
//! On Windows the child is `wr-capture`, which speaks the same JSON lines over
//! its stdin/stdout instead of a socket. Closing its stdin stands in for
//! SIGINT: it finishes any open file and exits.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::CaptureSettings;
#[cfg(unix)]
use crate::domain::ReplayStorage;
use crate::domain::{Category, Codec, RecordingId};
use crate::process;
use crate::storage::now_unix_ms;

/// Everything Recorder needs to arm a capture session, assembled by the
/// coordinator from validated configuration.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// GSR executable; the production value is `gpu-screen-recorder` on PATH.
    pub gsr_binary: PathBuf,
    /// App-private directory for the portal token and recorder log.
    pub data_dir: PathBuf,
    /// GSR's IPC socket. Unix socket paths are limited to 107 bytes, so this
    /// lives under the runtime directory rather than `data_dir`.
    pub ipc_socket: PathBuf,
    /// Capture root containing the `replay`, `regular`, and `staging`
    /// subdirectories.
    pub capture_root: PathBuf,
    pub settings: CaptureSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordingMode {
    Automatic,
    Manual,
    Test(Category),
}

#[derive(Clone, Debug)]
pub struct StartRequest {
    pub id: RecordingId,
    /// Detection delay plus lead-in, already clamped by the coordinator.
    pub requested_replay_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureArtifacts {
    /// GSR-saved replay pre-roll; missing falls back to regular-only.
    pub replay: Option<PathBuf>,
    pub regular: PathBuf,
    pub regular_started_at_ms: i64,
    pub regular_stopped_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureTargetSelection {
    /// Reusable portal token when GSR has already written it; otherwise `poll`
    /// reports it later as `TargetTokenAvailable`.
    pub token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AudioDevices {
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

#[derive(Debug)]
pub enum RecorderError {
    /// A recording is already active.
    Busy,
    /// No live armed GSR child.
    NotArmed,
    /// The supplied recording ID is not the active one.
    WrongId,
    /// GSR produced no regular recording within the bounded wait.
    MissingRegularArtifact,
    InvalidSettings(String),
    /// Portal selection was denied/cancelled (GSR exit code 60).
    SelectionDenied {
        log_tail: String,
    },
    SpawnFailed {
        message: String,
        log_tail: String,
    },
    Io(io::Error),
}

impl From<io::Error> for RecorderError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecorderEvent {
    ChildExited {
        code: Option<i32>,
    },
    RestartScheduled {
        attempt: u32,
    },
    Restarted,
    /// A requested end resolved: `None` means the bounded wait produced no
    /// regular recording, which the coordinator reports and sweeps.
    CaptureEnded {
        artifacts: Option<CaptureArtifacts>,
    },
    RestartFailed {
        message: String,
    },
    /// The portal wrote (or replaced) the reusable capture-target token.
    TargetTokenAvailable(String),
}

/// Bounded waits. Tests shrink them; production uses the defaults.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Post-spawn window in which GSR must stay alive and accept the IPC
    /// connection before arm is considered successful.
    pub arm_stability: Duration,
    /// Wait for the `save-replay` reply, counted from `begin`.
    pub replay_reply: Duration,
    /// Wait for the `stop-replay-recording` reply before escalating to SIGINT.
    pub stop_reply: Duration,
    /// Wait for the old child to exit during reselection/shutdown before
    /// escalating.
    pub exit_grace: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            arm_stability: Duration::from_millis(500),
            replay_reply: Duration::from_secs(20),
            stop_reply: Duration::from_secs(30),
            exit_grace: Duration::from_secs(2),
        }
    }
}

const GSR_EXIT_SELECTION_DENIED: i32 = 60;
const MAX_RESTART_DELAY_SECONDS: u64 = 30;
/// A child that ran this long before exiting was not crash-looping.
const STABLE_CHILD: Duration = Duration::from_secs(60);
/// The portal token only changes after a target selection, so `poll` need
/// not reread it every coordinator tick.
const TOKEN_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// GSR applies `start-replay-recording` on its next capture-loop iteration
/// and refuses a stop that arrives first. A refused stop is retried while the
/// start is younger than this; after that no recording exists.
const START_GRACE: Duration = Duration::from_secs(1);
const IPC_CONNECT_RETRY: Duration = Duration::from_millis(25);

/// The connection to GSR's `-ipc` socket. Requests are tiny and the socket is
/// otherwise idle, so writes on the nonblocking stream never wait.
struct Ipc {
    #[cfg(unix)]
    stream: UnixStream,
    /// `None` once closed to make the child finish and exit.
    #[cfg(windows)]
    stdin: Option<std::process::ChildStdin>,
    /// Stdout chunks from a reader thread, which disconnects at EOF.
    #[cfg(windows)]
    stdout: std::sync::mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    next_id: i64,
}

#[derive(Deserialize)]
struct Reply {
    id: i64,
    result: String,
    /// The saved path on success, the error message otherwise.
    data: Option<String>,
}

impl Ipc {
    #[cfg(windows)]
    fn attach(child: &mut Child) -> Option<Self> {
        let stdin = child.stdin.take()?;
        let mut stdout = child.stdout.take()?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read @ 1..) = stdout.read(&mut chunk) {
                if sender.send(chunk[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        Some(Self {
            stdin: Some(stdin),
            stdout: receiver,
            buf: Vec::new(),
            next_id: 0,
        })
    }

    #[cfg(unix)]
    fn connect(path: &Path) -> Option<Self> {
        let stream = UnixStream::connect(path).ok()?;
        stream.set_nonblocking(true).ok()?;
        Some(Self {
            stream,
            buf: Vec::new(),
            next_id: 0,
        })
    }

    fn send(&mut self, name: &str, data: Option<Value>) -> io::Result<i64> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({ "id": id, "name": name });
        if let Some(data) = data {
            request["data"] = data;
        }
        let mut line = request.to_string().into_bytes();
        line.push(b'\n');
        #[cfg(unix)]
        self.stream.write_all(&line)?;
        #[cfg(windows)]
        self.stdin
            .as_mut()
            .ok_or(io::ErrorKind::BrokenPipe)?
            .write_all(&line)?;
        tracing::info!(id, name, "sent GSR request");
        Ok(id)
    }

    /// Append every complete reply line to `replies`; `Err` once GSR closed
    /// the socket or it failed. Replies read before that are still appended.
    fn drain(&mut self, replies: &mut Vec<Reply>) -> io::Result<()> {
        let result = self.read_available();
        while let Some(end) = self.buf.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            match serde_json::from_slice::<Reply>(&line) {
                Ok(reply) => replies.push(reply),
                Err(error) => tracing::warn!(%error, "ignored a malformed GSR reply"),
            }
        }
        result
    }

    #[cfg(unix)]
    fn read_available(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(read) => self.buf.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(windows)]
    fn read_available(&mut self) -> io::Result<()> {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.stdout.try_recv() {
                Ok(chunk) => self.buf.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
            }
        }
    }
}

/// Ask the child to finish its files and exit: SIGINT for GSR, closing stdin
/// for `wr-capture`.
fn interrupt(child: &Child, ipc: Option<&mut Ipc>) {
    #[cfg(unix)]
    {
        let _ = ipc;
        let _ = process::send_signal(child, libc::SIGINT);
    }
    #[cfg(windows)]
    {
        let _ = child;
        if let Some(ipc) = ipc {
            ipc.stdin = None;
        }
    }
}

struct ActiveCapture {
    id: RecordingId,
    regular_started_at_ms: i64,
    begun_at: Instant,
    /// The unanswered `save-replay` request; `None` once it was answered or
    /// when the pre-roll is zero.
    save_id: Option<i64>,
    replay_deadline: Instant,
    replay: Option<PathBuf>,
}

/// A requested end whose replies have not all arrived yet. `poll` resolves it
/// so the coordinator thread never sleeps through GSR's flush and mux.
struct PendingEnd {
    active: ActiveCapture,
    stop_id: i64,
    stop_deadline: Instant,
    regular_stopped_at_ms: i64,
    regular: Option<PathBuf>,
    /// GSR refused the stop after `START_GRACE`: there is no recording.
    stop_failed: bool,
    sigint_sent: bool,
}

pub struct Recorder {
    config: Option<CaptureConfig>,
    child: Option<Child>,
    ipc: Option<Ipc>,
    spawned_at: Option<Instant>,
    desired_running: bool,
    restart_attempts: u32,
    restart_at_ms: Option<i64>,
    active: Option<ActiveCapture>,
    ending: Option<PendingEnd>,
    last_token: Option<String>,
    token_checked_at: Option<Instant>,
    timeouts: Timeouts,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        Self::with_timeouts(Timeouts::default())
    }

    pub fn with_timeouts(timeouts: Timeouts) -> Self {
        Self {
            config: None,
            child: None,
            ipc: None,
            spawned_at: None,
            desired_running: false,
            restart_attempts: 0,
            restart_at_ms: None,
            active: None,
            ending: None,
            last_token: None,
            token_checked_at: None,
            timeouts,
        }
    }

    fn token_path(config: &CaptureConfig) -> PathBuf {
        config.data_dir.join("gsr-portal.token")
    }

    fn log_path(config: &CaptureConfig) -> PathBuf {
        config.data_dir.join("gsr.log")
    }

    fn replay_dir(config: &CaptureConfig) -> PathBuf {
        config.capture_root.join("replay")
    }

    fn regular_dir(config: &CaptureConfig) -> PathBuf {
        config.capture_root.join("regular")
    }

    /// Validate GSR, prepare directories/log/token, spawn the replay buffer,
    /// and confirm it stays alive and connected. A deliberate arm resets the restart
    /// attempt counter.
    pub fn arm(&mut self, config: &CaptureConfig) -> Result<(), RecorderError> {
        if config.settings.audio_output.contains('|')
            || config
                .settings
                .audio_input
                .as_deref()
                .is_some_and(|input| input.contains('|'))
        {
            return Err(RecorderError::InvalidSettings(
                "audio device IDs must not contain '|'".to_string(),
            ));
        }
        // Check the replacement binary before touching a live capture. The
        // remaining setup errors occur only after the deliberate replacement
        // begins; invalid settings and an unavailable binary do not disarm.
        let version = process::command(&config.gsr_binary)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match version {
            Ok(status) if status.success() => {}
            Ok(status) => {
                return Err(RecorderError::SpawnFailed {
                    message: format!("gpu-screen-recorder --version failed: {status}"),
                    log_tail: String::new(),
                });
            }
            Err(error) => {
                return Err(RecorderError::SpawnFailed {
                    message: format!("gpu-screen-recorder not available: {error}"),
                    log_tail: String::new(),
                });
            }
        }
        self.desired_running = false;
        self.restart_at_ms = None;
        self.ipc = None;
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child, self.timeouts.exit_grace)?;
        }
        self.active = None;

        fs::create_dir_all(&config.data_dir)?;
        for dir in ["replay", "regular", "staging"] {
            fs::create_dir_all(config.capture_root.join(dir))?;
        }
        let managed = config.capture_root.join("managed.txt");
        if !managed.exists() {
            fs::write(
                &managed,
                "This folder is managed by Warcraft Recorder, files in it may be automatically created, modified or deleted.",
            )?;
        }

        // Leftovers of the hook protocol that preceded the IPC socket.
        let _ = fs::remove_file(config.data_dir.join("gsr-hook.sh"));
        let _ = fs::remove_file(config.data_dir.join("gsr-events.tsv"));
        fs::write(Self::log_path(config), b"")?;

        let token_path = Self::token_path(config);
        if let Some(token) = &config.settings.capture_target_token
            && !token.is_empty()
            && !token_path.exists()
        {
            fs::write(&token_path, token)?;
        }

        self.spawn_child(config)?;
        self.config = Some(config.clone());
        self.desired_running = true;
        self.restart_attempts = 0;
        self.restart_at_ms = None;
        self.last_token = read_token(&token_path);
        self.token_checked_at = None;
        Ok(())
    }

    fn spawn_child(&mut self, config: &CaptureConfig) -> Result<(), RecorderError> {
        // A child killed outright leaves its socket behind.
        match fs::remove_file(&config.ipc_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(Self::log_path(config))?;
        let mut command = process::command(&config.gsr_binary);
        // Stable Flatpak constrains the GTK process allocator arenas for its
        // RSS gate; the recorder must retain its own defaults.
        command.env_remove("MALLOC_ARENA_MAX");
        #[cfg(unix)]
        command
            .args(build_gsr_args(config))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?));
        #[cfg(windows)]
        command
            .args(build_wr_capture_args(config))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        command.stderr(Stdio::from(log));
        let mut child = command
            .spawn()
            .map_err(|error| RecorderError::SpawnFailed {
                message: format!("failed to spawn gpu-screen-recorder: {error}"),
                log_tail: String::new(),
            })?;

        // GSR binds the socket before the portal dialog, so connected does
        // not mean capturing; healthy is alive and connected at the deadline.
        let deadline = Instant::now() + self.timeouts.arm_stability;
        #[cfg(unix)]
        let mut ipc = None;
        #[cfg(windows)]
        let ipc = Ipc::attach(&mut child);
        loop {
            if let Some(status) = child.try_wait()? {
                let log_tail = process::read_log_tail(&Self::log_path(config));
                if status.code() == Some(GSR_EXIT_SELECTION_DENIED) {
                    return Err(RecorderError::SelectionDenied { log_tail });
                }
                return Err(RecorderError::SpawnFailed {
                    message: format!("gpu-screen-recorder exited immediately: {status}"),
                    log_tail,
                });
            }
            #[cfg(unix)]
            if ipc.is_none() {
                ipc = Ipc::connect(&config.ipc_socket);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(IPC_CONNECT_RETRY);
        }
        let Some(ipc) = ipc else {
            let _ = process::terminate(&mut child, self.timeouts.exit_grace);
            return Err(RecorderError::SpawnFailed {
                message: "gpu-screen-recorder IPC socket did not appear".to_string(),
                log_tail: process::read_log_tail(&Self::log_path(config)),
            });
        };
        self.child = Some(child);
        self.ipc = Some(ipc);
        self.spawned_at = Some(Instant::now());
        Ok(())
    }

    /// Save the pre-roll (whole seconds, none for a zero pre-roll) and start
    /// the regular recording. Never waits for media: the replies arrive
    /// through `poll`. Returns the regular recording's wall-clock start.
    pub fn begin(&mut self, request: StartRequest) -> Result<i64, RecorderError> {
        // A pending end still waits on replies of its own; the coordinator
        // defers the next capture instead, and this keeps that invariant
        // local to the recorder.
        if self.active.is_some() || self.ending.is_some() {
            return Err(RecorderError::Busy);
        }
        self.live_child()?;
        let regular_started_at_ms = now_unix_ms();
        let seconds = request.requested_replay_ms.div_ceil(1_000);
        let save_id = if seconds > 0 {
            Some(self.send("save-replay", Some(json!({ "seconds": seconds })))?)
        } else {
            None
        };
        self.send("start-replay-recording", None)?;
        let begun_at = Instant::now();
        self.active = Some(ActiveCapture {
            id: request.id,
            regular_started_at_ms,
            begun_at,
            save_id,
            replay_deadline: begun_at + self.timeouts.replay_reply,
            replay: None,
        });
        Ok(regular_started_at_ms)
    }

    /// Stop the regular recording and resolve its artifacts through `poll`.
    /// GSR needs however long the encoder flush and mux take, and the
    /// coordinator owns every piece of UI state while it waits, so the wait
    /// must not happen here. Missing replay stays tolerated; a missing regular
    /// recording arrives as `CaptureEnded { artifacts: None }`.
    ///
    /// Discarding a capture uses the same request: the coordinator sweeps the
    /// artifacts instead of finalizing them. Recorder never unlinks them.
    pub fn request_end(&mut self, id: &RecordingId) -> Result<(), RecorderError> {
        if self.ending.is_some() {
            return Err(RecorderError::Busy);
        }
        match &self.active {
            None => return Err(RecorderError::NotArmed),
            Some(active) if &active.id != id => return Err(RecorderError::WrongId),
            Some(_) => {}
        }
        self.live_child()?;
        let regular_stopped_at_ms = now_unix_ms();
        let stop_id = self.send("stop-replay-recording", None)?;
        let active = self.active.take().expect("checked above");
        self.ending = Some(PendingEnd {
            active,
            stop_id,
            stop_deadline: Instant::now() + self.timeouts.stop_reply,
            regular_stopped_at_ms,
            regular: None,
            stop_failed: false,
            sigint_sent: false,
        });
        Ok(())
    }

    /// Send one request. A failed write means GSR is gone or wedged, which is
    /// handled like its death.
    fn send(&mut self, name: &str, data: Option<Value>) -> Result<i64, RecorderError> {
        let ipc = self.ipc.as_mut().ok_or(RecorderError::NotArmed)?;
        ipc.send(name, data).map_err(|error| {
            self.lose_ipc();
            error.into()
        })
    }

    /// GSR only closes a client when it exits or the client misbehaves, so a
    /// child still alive behind a dead socket is wedged: SIGINT it and let
    /// the exit branch of `poll` restart it.
    fn lose_ipc(&mut self) {
        let mut ipc = self.ipc.take();
        if let Some(child) = self.child.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            interrupt(child, ipc.as_mut());
        }
    }

    /// Whether a replay-buffer child is currently available.
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// True between `request_end` and its `CaptureEnded`.
    pub fn is_ending(&self) -> bool {
        self.ending.is_some()
    }

    /// Shutdown only: drive the requested end to its conclusion so the
    /// finalization is queued before GSR is killed.
    ///
    /// `deadline` keeps quitting responsive. The full 30 s stop wait is
    /// right for a running app but not for a window the user just closed: a
    /// quit that appears hung invites a force-kill, which orphans GSR and
    /// leaves it capturing the screen forever. On expiry the end resolves with
    /// whatever arrived, exactly as a timed-out poll would.
    pub fn finish_end_blocking(&mut self, deadline: Instant) -> Vec<RecorderEvent> {
        let mut events = Vec::new();
        while self.ending.is_some() {
            self.poll_pending_end(&mut events);
            if self.ending.is_none() {
                break;
            }
            if Instant::now() >= deadline {
                self.resolve_end(&mut events);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        events
    }

    /// One nonblocking step of a requested end. The regular recording is
    /// awaited first against its own deadline, then the replay pre-roll
    /// against the absolute deadline taken at `begin`, if one was requested.
    fn poll_pending_end(&mut self, events: &mut Vec<RecorderEvent>) {
        self.dispatch_replies();
        let now = Instant::now();
        let Some(ending) = self.ending.as_mut() else {
            return;
        };
        if ending.regular.is_none() && !ending.stop_failed {
            if now < ending.stop_deadline {
                return;
            }
            // No reply: SIGINT finalizes a running recording and still
            // answers the stop. Its exit resolves this end, and poll reports
            // that first so the coordinator disarms before a deferred capture
            // can start on a child that is going away.
            if !ending.sigint_sent {
                ending.sigint_sent = true;
                ending.stop_deadline = now + self.timeouts.exit_grace;
                if let Some(child) = self.child.as_ref() {
                    interrupt(child, self.ipc.as_mut());
                }
                return;
            }
        } else if ending.regular.is_some()
            && ending.active.save_id.is_some()
            && now < ending.active.replay_deadline
        {
            return;
        }
        self.resolve_end(events);
    }

    /// Resolve the pending end with whatever arrived. A save reply that
    /// arrives later is ignored by its id; the sweep removes its file.
    fn resolve_end(&mut self, events: &mut Vec<RecorderEvent>) {
        let Some(ending) = self.ending.take() else {
            return;
        };
        let artifacts = ending.regular.map(|regular| CaptureArtifacts {
            replay: ending.active.replay,
            regular,
            regular_started_at_ms: ending.active.regular_started_at_ms,
            regular_stopped_at_ms: ending.regular_stopped_at_ms,
        });
        events.push(RecorderEvent::CaptureEnded { artifacts });
    }

    /// Apply GSR's replies to the active capture or the pending end. `poll`
    /// runs this before reading the child's exit status, so a reply flushed
    /// while GSR exits is not lost.
    fn dispatch_replies(&mut self) {
        let Some(ipc) = self.ipc.as_mut() else {
            return;
        };
        let mut replies = Vec::new();
        let drained = ipc.drain(&mut replies);
        for reply in replies {
            self.apply_reply(reply);
        }
        if let Err(error) = drained {
            tracing::warn!(%error, "GSR IPC connection lost");
            self.lose_ipc();
        }
    }

    fn apply_reply(&mut self, reply: Reply) {
        let ok = reply.result == "ok";
        tracing::info!(
            id = reply.id,
            result = %reply.result,
            data = reply.data.as_deref().unwrap_or(""),
            "GSR reply"
        );
        // The save can answer before or after the end was requested.
        let capture = match (self.active.as_mut(), self.ending.as_mut()) {
            (Some(active), _) => Some(active),
            (None, Some(ending)) => Some(&mut ending.active),
            (None, None) => None,
        };
        if let Some(capture) = capture
            && capture.save_id == Some(reply.id)
        {
            capture.save_id = None;
            capture.replay = reply.data.filter(|_| ok).map(PathBuf::from);
            return;
        }
        let Some(ending) = self
            .ending
            .as_mut()
            .filter(|ending| ending.stop_id == reply.id)
        else {
            return;
        };
        if let (true, Some(path)) = (ok, reply.data) {
            ending.regular = Some(PathBuf::from(path));
        } else if ending.active.begun_at.elapsed() < START_GRACE {
            ending.regular_stopped_at_ms = now_unix_ms();
            if let Ok(id) = self.send("stop-replay-recording", None)
                && let Some(ending) = self.ending.as_mut()
            {
                ending.stop_id = id;
            }
        } else {
            ending.stop_failed = true;
        }
    }

    /// Token contract: stop the child, invalidate the token only after it
    /// exited, and re-arm to trigger portal selection. A denied selection
    /// restores the previous usable token.
    pub fn reselect_target(
        &mut self,
        config: &CaptureConfig,
    ) -> Result<CaptureTargetSelection, RecorderError> {
        let token_path = Self::token_path(config);
        self.ipc = None;
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child, self.timeouts.exit_grace)?;
        }
        let previous = read_token(&token_path);
        match fs::remove_file(&token_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Do not let arm restore the token we just invalidated. The portal
        // must select a new target; once it writes the replacement token,
        // poll reports it to the coordinator.
        let mut reselect_config = config.clone();
        reselect_config.settings.capture_target_token = None;
        match self.arm(&reselect_config) {
            Ok(()) => {
                let token = read_token(&token_path);
                self.last_token = token.clone();
                Ok(CaptureTargetSelection { token })
            }
            Err(error) => {
                // Cancellation preserves the prior usable target.
                if let Some(previous) = previous {
                    let _ = fs::write(&token_path, &previous);
                    let _ = self.arm(config);
                }
                Err(error)
            }
        }
    }

    /// `gpu-screen-recorder --list-audio-devices` with the recorded 2 s
    /// timeout; defaults are always present.
    #[cfg(windows)]
    pub fn audio_devices(&mut self) -> Result<AudioDevices, RecorderError> {
        let device = |id: &str, label: &str| AudioDevice {
            id: id.to_string(),
            label: label.to_string(),
        };
        Ok(AudioDevices {
            outputs: vec![
                device("default_output", "Desktop audio"),
                device("app", "Game audio only"),
            ],
            inputs: vec![device("default_input", "Default microphone")],
        })
    }

    #[cfg(unix)]
    pub fn audio_devices(&mut self) -> Result<AudioDevices, RecorderError> {
        let binary = self
            .config
            .as_ref()
            .map(|config| config.gsr_binary.clone())
            .unwrap_or_else(|| PathBuf::from("gpu-screen-recorder"));
        let mut child = process::command(binary)
            .env_remove("MALLOC_ARENA_MAX")
            .arg("--list-audio-devices")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RecorderError::SpawnFailed {
                message: format!("audio discovery failed: {error}"),
                log_tail: String::new(),
            })?;
        let timed_out = !process::wait_with_timeout(&mut child, Duration::from_secs(2))?;
        let status = if timed_out {
            // The process may have exited between the timeout check and the
            // escalation; avoid reporting a spurious kill failure in that
            // race and still collect its final status.
            match child.try_wait()? {
                Some(status) => status,
                None => {
                    child.kill()?;
                    child.wait()?
                }
            }
        } else {
            child
                .try_wait()?
                .ok_or_else(|| io::Error::other("audio discovery exited without a status"))?
        };
        let mut text = String::new();
        if let Some(stdout) = child.stdout.as_mut() {
            stdout.read_to_string(&mut text)?;
        }
        text.push('\n');
        if let Some(stderr) = child.stderr.as_mut() {
            stderr.read_to_string(&mut text)?;
        }
        if timed_out {
            return Err(RecorderError::SpawnFailed {
                message: "audio discovery timed out after 2 seconds".to_string(),
                log_tail: text_tail(&text),
            });
        }
        if !status.success() {
            return Err(RecorderError::SpawnFailed {
                message: format!("audio discovery exited unsuccessfully: {status}"),
                log_tail: text_tail(&text),
            });
        }
        Ok(parse_audio_devices(&text))
    }

    /// Advance restarts and surface child/token changes. The coordinator
    /// calls this on its loop; Recorder keeps no timer thread.
    pub fn poll(&mut self, now_ms: i64) -> Vec<RecorderEvent> {
        let mut events = Vec::new();
        self.dispatch_replies();
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.child = None;
                    self.ipc = None;
                    self.active = None;
                    // A capture waiting to be written will never be: nothing
                    // is left to write it. Expire both waits so
                    // `poll_pending_end` resolves in this same batch, instead
                    // of holding the coordinator's ending state, and the
                    // recovery actions it blocks, against a dead child. The
                    // replay wait matters too: a child that answered the stop
                    // and then died would otherwise keep the end open for the
                    // rest of its pre-roll wait.
                    if let Some(ending) = self.ending.as_mut() {
                        ending.stop_deadline = Instant::now();
                        ending.active.save_id = None;
                        ending.sigint_sent = true;
                    }
                    events.push(RecorderEvent::ChildExited {
                        code: status.code(),
                    });
                    if self
                        .spawned_at
                        .is_some_and(|spawned_at| spawned_at.elapsed() >= STABLE_CHILD)
                    {
                        self.restart_attempts = 0;
                    }
                    if self.desired_running {
                        self.schedule_restart(now_ms, &mut events);
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "GSR child status check failed"),
            }
        } else if self.desired_running
            && self
                .restart_at_ms
                .is_some_and(|deadline| now_ms >= deadline)
        {
            self.restart_at_ms = None;
            let config = self.config.clone().expect("desired_running implies config");
            match self.spawn_child(&config) {
                // The attempt counter survives the respawn itself; it resets
                // once this child proves stable (see the exit branch above).
                Ok(()) => events.push(RecorderEvent::Restarted),
                Err(error) => {
                    events.push(RecorderEvent::RestartFailed {
                        message: format!("{error:?}"),
                    });
                    self.schedule_restart(now_ms, &mut events);
                }
            }
        }

        if let Some(config) = &self.config
            && self
                .token_checked_at
                .is_none_or(|checked_at| checked_at.elapsed() >= TOKEN_CHECK_INTERVAL)
        {
            self.token_checked_at = Some(Instant::now());
            if let Some(token) = read_token(&Self::token_path(config))
                && self.last_token.as_deref() != Some(token.as_str())
            {
                self.last_token = Some(token.clone());
                events.push(RecorderEvent::TargetTokenAvailable(token));
            }
        }
        self.poll_pending_end(&mut events);
        events
    }

    fn schedule_restart(&mut self, now_ms: i64, events: &mut Vec<RecorderEvent>) {
        self.restart_attempts += 1;
        let delay_seconds = MAX_RESTART_DELAY_SECONDS.min(1u64 << self.restart_attempts.min(63));
        self.restart_at_ms = Some(now_ms + (delay_seconds * 1_000) as i64);
        events.push(RecorderEvent::RestartScheduled {
            attempt: self.restart_attempts,
        });
    }

    /// Stop the replay child with the SIGINT-then-kill escalation and leave no
    /// child or scheduled restart behind.
    pub fn shutdown(&mut self) -> Result<(), RecorderError> {
        self.desired_running = false;
        self.restart_at_ms = None;
        self.active = None;
        self.ending = None;
        self.ipc = None;
        if let Some(mut child) = self.child.take() {
            process::terminate(&mut child, self.timeouts.exit_grace)?;
        }
        Ok(())
    }

    fn live_child(&mut self) -> Result<&Child, RecorderError> {
        let alive = match self.child.as_mut() {
            Some(child) => child.try_wait()?.is_none(),
            None => false,
        };
        if alive && self.ipc.is_some() {
            Ok(self.child.as_ref().expect("alive"))
        } else {
            Err(RecorderError::NotArmed)
        }
    }
}

/// `wr-capture` finds the WoW window itself and only knows RAM replay.
#[cfg(windows)]
fn build_wr_capture_args(config: &CaptureConfig) -> Vec<OsString> {
    let settings = &config.settings;
    let codec = match settings.codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    };
    let mut args: Vec<OsString> = vec![
        "-w".into(),
        "Wow.exe".into(),
        "-r".into(),
        settings.replay_buffer_seconds.to_string().into(),
        "-f".into(),
        settings.fps.to_string().into(),
        "-q".into(),
        settings.bitrate_kbps.to_string().into(),
        "-k".into(),
        codec.into(),
        "-cursor".into(),
        if settings.capture_cursor { "yes" } else { "no" }.into(),
        "-o".into(),
        Recorder::replay_dir(config).into_os_string(),
        "-ro".into(),
        Recorder::regular_dir(config).into_os_string(),
    ];
    let audio = audio_sources(settings);
    if !audio.is_empty() {
        args.push("-a".into());
        args.push(audio.join("|").into());
    }
    args
}

fn audio_sources(settings: &CaptureSettings) -> Vec<&str> {
    let mut audio: Vec<&str> = Vec::new();
    if !settings.audio_output.is_empty() {
        audio.push(settings.audio_output.as_str());
    }
    if let Some(input) = settings.audio_input.as_deref()
        && !input.is_empty()
        && !audio.contains(&input)
    {
        audio.push(input);
    }
    audio
}

/// Paths and devices stay single `OsString` arguments; no shell is involved.
#[cfg(unix)]
fn build_gsr_args(config: &CaptureConfig) -> Vec<OsString> {
    let settings = &config.settings;
    let codec = match settings.codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    };
    let storage = match settings.replay_storage {
        ReplayStorage::Ram => "ram",
        ReplayStorage::Disk => "disk",
    };
    let mut args: Vec<OsString> = vec![
        "-w".into(),
        "portal".into(),
        "-restore-portal-session".into(),
        "yes".into(),
        "-portal-session-token-filepath".into(),
        Recorder::token_path(config).into_os_string(),
        "-r".into(),
        settings.replay_buffer_seconds.to_string().into(),
        "-replay-storage".into(),
        storage.into(),
        "-restart-replay-on-save".into(),
        "no".into(),
        "-c".into(),
        "mkv".into(),
        "-f".into(),
        settings.fps.to_string().into(),
        "-bm".into(),
        "cbr".into(),
        "-q".into(),
        settings.bitrate_kbps.to_string().into(),
        "-k".into(),
        codec.into(),
        "-ac".into(),
        "aac".into(),
        "-cursor".into(),
        if settings.capture_cursor { "yes" } else { "no" }.into(),
        "-o".into(),
        Recorder::replay_dir(config).into_os_string(),
        "-ro".into(),
        Recorder::regular_dir(config).into_os_string(),
        "-ipc".into(),
        config.ipc_socket.clone().into_os_string(),
        "-v".into(),
        "no".into(),
    ];
    let audio = audio_sources(settings);
    if !audio.is_empty() {
        args.push("-a".into());
        args.push(audio.join("|").into());
    }
    args
}

fn read_token(path: &Path) -> Option<String> {
    let token = fs::read_to_string(path).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

#[cfg(unix)]
fn text_tail(text: &str) -> String {
    let start = text
        .len()
        .saturating_sub(usize::try_from(process::LOG_TAIL_BYTES).unwrap_or(usize::MAX));
    String::from_utf8_lossy(&text.as_bytes()[start..]).into_owned()
}

/// `--list-audio-devices` prints one `name|description` line per source. The
/// defaults always come first; every other source is passed to `-a` as
/// `device:<name>` and sorted by its PulseAudio/PipeWire name: `*.monitor`
/// sources are outputs, everything else (virtual mics included) is an input.
#[cfg_attr(windows, allow(dead_code))]
fn parse_audio_devices(text: &str) -> AudioDevices {
    let device = |id: &str, detail: &str| AudioDevice {
        id: id.to_string(),
        label: if detail.is_empty() { id } else { detail }.to_string(),
    };
    let mut devices = AudioDevices {
        outputs: vec![device("default_output", "Default output device")],
        inputs: vec![device("default_input", "Default input device")],
    };
    for line in text.lines() {
        let Some((name, detail)) = line.split_once('|') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.starts_with("default_") {
            continue;
        }
        let list = if name.ends_with(".monitor") {
            &mut devices.outputs
        } else {
            &mut devices.inputs
        };
        let id = format!("device:{name}");
        if !list.iter().any(|existing| existing.id == id) {
            list.push(device(&id, detail.trim()));
        }
    }
    devices
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fake_gsr() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/native/bin/fake-gsr.py")
    }

    fn test_timeouts() -> Timeouts {
        Timeouts {
            arm_stability: Duration::from_millis(150),
            replay_reply: Duration::from_millis(300),
            stop_reply: Duration::from_millis(300),
            exit_grace: Duration::from_millis(500),
        }
    }

    fn test_config(name: &str) -> CaptureConfig {
        let root = crate::storage::test_root(&format!("recorder-{name}"));
        let data_dir = root.join("data dir with späce");
        CaptureConfig {
            gsr_binary: fake_gsr(),
            ipc_socket: data_dir.join("gsr.sock"),
            data_dir,
            capture_root: root.join("capture"),
            settings: CaptureSettings::default(),
        }
    }

    /// Every request the fake GSR received.
    fn requests(config: &CaptureConfig) -> Vec<Value> {
        fs::read_to_string(config.data_dir.join("fake-requests"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn end_artifacts(recorder: &mut Recorder) -> Option<CaptureArtifacts> {
        recorder
            .finish_end_blocking(Instant::now() + Duration::from_secs(5))
            .into_iter()
            .find_map(|event| match event {
                RecorderEvent::CaptureEnded { artifacts } => Some(artifacts),
                _ => None,
            })
            .expect("a requested end always resolves")
    }

    #[test]
    fn argv_preserves_awkward_paths_and_joins_audio_sources() {
        let mut config = test_config("argv");
        config.settings.audio_output = "device:out put".to_string();
        config.settings.audio_input = Some("device:mic".to_string());
        let args = build_gsr_args(&config);
        // The data dir contains a space and a non-ASCII byte; it stays one arg.
        let ipc = args.iter().position(|arg| arg == "-ipc").unwrap();
        assert_eq!(args[ipc + 1], config.data_dir.join("gsr.sock"));
        assert!(!args.contains(&OsString::from("-sc")));
        assert_eq!(
            args[args.len() - 2..],
            ["-a".into(), "device:out put|device:mic".into()] as [OsString; 2]
        );

        // Duplicate input collapses; empty audio drops -a entirely.
        config.settings.audio_input = Some("device:out put".to_string());
        let args = build_gsr_args(&config);
        assert_eq!(args.last().unwrap(), &OsString::from("device:out put"));
        config.settings.audio_output = String::new();
        config.settings.audio_input = None;
        let args = build_gsr_args(&config);
        assert!(!args.contains(&OsString::from("-a")));
    }

    #[test]
    fn lifecycle_saves_exact_seconds_and_returns_both_paths() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        let config = test_config("lifecycle");
        recorder.arm(&config).unwrap();

        let id = RecordingId::new();
        recorder
            .begin(StartRequest {
                id: id.clone(),
                requested_replay_ms: 12_345,
            })
            .unwrap();
        // A second begin cannot disturb the active session.
        assert!(matches!(
            recorder.begin(StartRequest {
                id: RecordingId::new(),
                requested_replay_ms: 0,
            }),
            Err(RecorderError::Busy)
        ));
        // Ending the wrong ID is rejected and the session stays live.
        assert!(matches!(
            recorder.request_end(&RecordingId::new()),
            Err(RecorderError::WrongId)
        ));

        // GSR has not applied the start yet, so it refuses this stop; the
        // recorder retries until it is accepted.
        recorder.request_end(&id).unwrap();
        assert!(recorder.is_ending());
        let artifacts = end_artifacts(&mut recorder).expect("regular artifact");
        assert_eq!(
            artifacts.replay,
            Some(Recorder::replay_dir(&config).join("Replay_1.mkv"))
        );
        assert_eq!(
            artifacts.regular,
            Recorder::regular_dir(&config).join("Video_1.mkv")
        );
        assert!(artifacts.regular_stopped_at_ms >= artifacts.regular_started_at_ms);
        let requests = requests(&config);
        assert_eq!(requests[0]["name"], "save-replay");
        assert_eq!(requests[0]["data"]["seconds"], 13);
        assert_eq!(requests[1]["name"], "start-replay-recording");
        let stops = requests
            .iter()
            .filter(|request| request["name"] == "stop-replay-recording")
            .count();
        assert!(stops > 1, "the refused stop was not retried: {requests:?}");
        recorder.shutdown().unwrap();
    }

    #[test]
    fn zero_pre_roll_sends_no_save() {
        let mut recorder = Recorder::with_timeouts(Timeouts {
            replay_reply: Duration::from_secs(60),
            ..test_timeouts()
        });
        let config = test_config("no-replay");
        recorder.arm(&config).unwrap();

        let id = RecordingId::new();
        recorder
            .begin(StartRequest {
                id: id.clone(),
                requested_replay_ms: 0,
            })
            .unwrap();
        recorder.request_end(&id).unwrap();
        // The stop reply alone resolves it; nothing waits out the replay wait
        // or `end_artifacts`' own 5 s bound.
        let started = Instant::now();
        let artifacts = end_artifacts(&mut recorder).expect("regular artifact");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(artifacts.replay, None);
        assert_eq!(
            artifacts.regular,
            Recorder::regular_dir(&config).join("Video_1.mkv")
        );
        assert!(
            requests(&config)
                .iter()
                .all(|request| request["name"] != "save-replay")
        );
        recorder.shutdown().unwrap();
    }

    #[test]
    fn save_error_is_regular_only_and_a_stuck_stop_replaces_the_child() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        let config = test_config("missing");
        recorder.arm(&config).unwrap();

        fs::write(config.data_dir.join("fake-no-replay"), "").unwrap();
        let id = RecordingId::new();
        recorder
            .begin(StartRequest {
                id: id.clone(),
                requested_replay_ms: 5_000,
            })
            .unwrap();
        recorder.request_end(&id).unwrap();
        let artifacts = end_artifacts(&mut recorder).expect("regular artifact");
        assert_eq!(artifacts.replay, None);

        // A stop GSR never answers escalates to SIGINT, so the child goes.
        fs::write(config.data_dir.join("fake-hold"), "").unwrap();
        let id = RecordingId::new();
        recorder
            .begin(StartRequest {
                id: id.clone(),
                requested_replay_ms: 0,
            })
            .unwrap();
        recorder.request_end(&id).unwrap();
        assert!(end_artifacts(&mut recorder).is_none());
        assert!(matches!(
            recorder.begin(StartRequest {
                id: RecordingId::new(),
                requested_replay_ms: 0,
            }),
            Err(RecorderError::NotArmed)
        ));
        recorder.shutdown().unwrap();
    }

    #[test]
    fn restart_schedule_caps_resets_and_stops() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        let config = test_config("restart");
        recorder.arm(&config).unwrap();

        let mut now_ms = 1_000_000i64;
        let mut delays = Vec::new();
        for _ in 0..6 {
            // Kill the child and observe the scheduled delay.
            process::send_signal(recorder.child.as_ref().unwrap(), libc::SIGKILL).unwrap();
            recorder.child.as_mut().unwrap().wait().unwrap();
            let events = recorder.poll(now_ms);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, RecorderEvent::ChildExited { .. }))
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, RecorderEvent::RestartScheduled { .. }))
            );
            let delay = recorder.restart_at_ms.expect("restart scheduled") - now_ms;
            delays.push(delay);
            // Nothing happens before the deadline.
            assert!(recorder.poll(now_ms + delay - 1).is_empty());
            now_ms += delay;
            let events = recorder.poll(now_ms);
            assert!(
                events.contains(&RecorderEvent::Restarted),
                "expected restart, got {events:?}"
            );
        }
        assert_eq!(delays, vec![2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);

        // A failed automatic respawn keeps retrying with the capped delay;
        // removing the failure marker allows the next scheduled attempt to
        // recover without resetting the attempt counter.
        fs::write(config.data_dir.join("fake-exit"), "1").unwrap();
        process::send_signal(recorder.child.as_ref().unwrap(), libc::SIGKILL).unwrap();
        recorder.child.as_mut().unwrap().wait().unwrap();
        let events = recorder.poll(now_ms);
        assert!(events.contains(&RecorderEvent::RestartScheduled { attempt: 7 }));
        assert_eq!(recorder.restart_at_ms, Some(now_ms + 30_000));
        let events = recorder.poll(now_ms + 30_000);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, RecorderEvent::RestartFailed { .. }))
        );
        assert!(events.contains(&RecorderEvent::RestartScheduled { attempt: 8 }));
        let retry_at = recorder.restart_at_ms;
        assert_eq!(retry_at, Some(now_ms + 60_000));
        fs::remove_file(config.data_dir.join("fake-exit")).unwrap();
        assert!(
            recorder
                .poll(retry_at.unwrap())
                .contains(&RecorderEvent::Restarted)
        );

        // A child that stayed up long enough starts the backoff over.
        let now_ms = retry_at.unwrap();
        recorder.spawned_at = Instant::now().checked_sub(STABLE_CHILD);
        process::send_signal(recorder.child.as_ref().unwrap(), libc::SIGKILL).unwrap();
        recorder.child.as_mut().unwrap().wait().unwrap();
        let events = recorder.poll(now_ms);
        assert!(events.contains(&RecorderEvent::RestartScheduled { attempt: 1 }));
        assert_eq!(recorder.restart_at_ms, Some(now_ms + 2_000));
        assert!(
            recorder
                .poll(now_ms + 2_000)
                .contains(&RecorderEvent::Restarted)
        );

        // A deliberate arm resets the attempt counter.
        recorder.arm(&config).unwrap();
        process::send_signal(recorder.child.as_ref().unwrap(), libc::SIGKILL).unwrap();
        recorder.child.as_mut().unwrap().wait().unwrap();
        let events = recorder.poll(now_ms);
        assert!(events.contains(&RecorderEvent::RestartScheduled { attempt: 1 }));
        assert_eq!(recorder.restart_at_ms, Some(now_ms + 2_000));

        // Shutdown cancels the pending restart and leaves no child.
        recorder.shutdown().unwrap();
        assert!(recorder.poll(now_ms + 60_000).is_empty());
        assert!(recorder.child.is_none());
    }

    #[test]
    fn reselection_rotates_the_token_and_denial_restores_it() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        let mut config = test_config("reselect");
        config.settings.capture_target_token = Some("configured-old-token".to_string());
        recorder.arm(&config).unwrap();
        let token_path = Recorder::token_path(&config);
        fs::write(&token_path, "old-token").unwrap();
        // Make poll adopt the current token first.
        recorder.poll(now_unix_ms());

        let selection = recorder.reselect_target(&config).unwrap();
        assert!(recorder.is_running());
        // The old token was deleted; GSR has not written a new one yet.
        assert_eq!(selection.token, None);
        assert!(!token_path.exists());
        // The portal writes the new token later; poll reports it.
        fs::write(&token_path, "new-token").unwrap();
        let events = recorder.poll(now_unix_ms());
        assert!(events.contains(&RecorderEvent::TargetTokenAvailable(
            "new-token".to_string()
        )));

        // A denied reselection restores the previous usable token.
        fs::write(config.data_dir.join("fake-exit"), "60").unwrap();
        let denied = recorder.reselect_target(&config);
        assert!(matches!(denied, Err(RecorderError::SelectionDenied { .. })));
        assert_eq!(fs::read_to_string(&token_path).unwrap(), "new-token");
        fs::remove_file(config.data_dir.join("fake-exit")).unwrap();
        recorder.shutdown().unwrap();
    }

    #[test]
    fn audio_discovery_maps_gsr_sources_to_device_ids() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        recorder.arm(&test_config("audio")).unwrap();
        let devices = recorder.audio_devices().unwrap();
        let ids = |list: &[AudioDevice]| list.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&devices.outputs),
            [
                "default_output",
                "device:alsa_output.pci.analog-stereo.monitor"
            ]
        );
        assert_eq!(
            ids(&devices.inputs),
            ["default_input", "device:alsa_input.usb-mic"]
        );
        assert_eq!(devices.inputs[1].label, "Fake USB Microphone");
        assert_eq!(devices.outputs[0].label, "Default output device");
        recorder.shutdown().unwrap();

        // Duplicates collapse, lines without `|` are ignored, an empty
        // description falls back to the id, and a virtual mic is an input.
        let parsed = parse_audio_devices(
            "bluez_output.x.monitor|Headset\nbluez_output.x.monitor|Headset\nnoise\nbluez_output.y.monitor|\neasyeffects_source|Easy Effects Source\n",
        );
        assert_eq!(
            ids(&parsed.outputs),
            [
                "default_output",
                "device:bluez_output.x.monitor",
                "device:bluez_output.y.monitor"
            ]
        );
        assert_eq!(parsed.outputs[2].label, "device:bluez_output.y.monitor");
        assert_eq!(
            ids(&parsed.inputs),
            ["default_input", "device:easyeffects_source"]
        );
    }

    #[test]
    fn begin_requires_a_live_armed_child() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        assert!(matches!(
            recorder.begin(StartRequest {
                id: RecordingId::new(),
                requested_replay_ms: 0,
            }),
            Err(RecorderError::NotArmed)
        ));
        assert!(matches!(
            recorder.request_end(&RecordingId::new()),
            Err(RecorderError::NotArmed)
        ));
    }

    #[test]
    fn invalid_arm_does_not_disarm_existing_capture() {
        let mut recorder = Recorder::with_timeouts(test_timeouts());
        let config = test_config("invalid-live");
        recorder.arm(&config).unwrap();

        let mut invalid = config.clone();
        invalid.settings.audio_output = "a|b".to_string();
        assert!(matches!(
            recorder.arm(&invalid),
            Err(RecorderError::InvalidSettings(_))
        ));
        assert!(recorder.is_running());
        recorder
            .begin(StartRequest {
                id: RecordingId::new(),
                requested_replay_ms: 0,
            })
            .unwrap();
        recorder.shutdown().unwrap();
    }
}
