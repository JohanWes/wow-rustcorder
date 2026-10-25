// SPDX-License-Identifier: GPL-3.0-or-later

//! Serialized media and storage jobs.
//!
//! One worker thread consumes `MediaJob`s in order: finalizing a finished
//! capture, or cutting a clip. FFmpeg is
//! spawned directly and polled through its `-progress` file plus an exclusive
//! stderr log. Shutdown arrives on a capacity-one control channel: automatic
//! finalization gets a bounded grace period, user clips are cancelled at once,
//! and both escalate SIGINT then `Child::kill`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::activity::RecordingDraft;
use crate::domain::{
    ActivityDetails, Category, LibraryEntry, MediaFacts, MeterData, RecordingId, TimelineItem,
    TimelineShape, WorkKind, WorkProgress,
};
use crate::process;
use crate::recorder::CaptureArtifacts;
use crate::storage::{CombinedMedia, Storage, now_unix_ms, sanitize_name, unique_stem};

/// Emitting progress more often is visually indistinguishable but makes every
/// GTK snapshot repeat work.
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(250);
/// An idle worker only waits for jobs (which wake it at once) and shutdown,
/// so it need not check the control channel every poll interval.
const IDLE_WAIT: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub struct MediaConfig {
    pub ffmpeg: PathBuf,
    /// Local UTC offset for generated display names, supplied by the coordinator.
    pub utc_offset_minutes: i32,
    /// Poll interval for control and FFmpeg progress while FFmpeg runs.
    pub poll_interval: Duration,
    /// Grace an in-flight automatic finalization gets after a shutdown request.
    pub finalize_grace: Duration,
    /// Grace a SIGINT is given before `Child::kill`.
    pub sigint_grace: Duration,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            ffmpeg: PathBuf::from("ffmpeg"),
            utc_offset_minutes: 0,
            poll_interval: Duration::from_millis(50),
            finalize_grace: Duration::from_secs(30),
            sigint_grace: Duration::from_secs(2),
        }
    }
}

pub enum MediaJob {
    FinalizeRecording {
        draft: Box<RecordingDraft>,
        artifacts: CaptureArtifacts,
        facts: MediaFacts,
    },
    CreateClip {
        source: Box<LibraryEntry>,
        start_ms: u64,
        end_ms: u64,
    },
}

impl MediaJob {
    pub fn kind(&self) -> WorkKind {
        match self {
            Self::FinalizeRecording { .. } => WorkKind::Finalize,
            Self::CreateClip { .. } => WorkKind::Clip,
        }
    }
}

pub enum MediaControl {
    Shutdown {
        pending_finalizations: Vec<MediaJob>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaEvent {
    Progress(WorkProgress),
    Completed {
        kind: WorkKind,
        entry: Box<LibraryEntry>,
    },
    /// The recording produced no library entry (discarded draft, failed job).
    Failed {
        kind: WorkKind,
        message: String,
    },
    Cancelled {
        kind: WorkKind,
    },
}

pub struct MediaWorker {
    config: MediaConfig,
    storage: Storage,
    jobs: Receiver<MediaJob>,
    control: Receiver<MediaControl>,
    events: Sender<MediaEvent>,
    shutdown_at: Option<Instant>,
    shutdown_finalizations: std::collections::VecDeque<MediaJob>,
}

impl MediaWorker {
    pub fn new(
        config: MediaConfig,
        storage: Storage,
        jobs: Receiver<MediaJob>,
        control: Receiver<MediaControl>,
        events: Sender<MediaEvent>,
    ) -> Self {
        Self {
            config,
            storage,
            jobs,
            control,
            events,
            shutdown_at: None,
            shutdown_finalizations: std::collections::VecDeque::new(),
        }
    }

    /// Consume jobs until shutdown is requested or the job channel closes.
    pub fn run(mut self) {
        loop {
            if self.shutdown_at.is_some() {
                while let Some(job) = self.shutdown_finalizations.pop_front() {
                    self.run_job(job);
                }
                break;
            }
            match self.jobs.recv_timeout(IDLE_WAIT) {
                Ok(job) => self.run_job(job),
                Err(RecvTimeoutError::Timeout) => self.observe_shutdown(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn observe_shutdown(&mut self) {
        if self.shutdown_at.is_some() {
            return;
        }
        match self.control.try_recv() {
            Ok(MediaControl::Shutdown {
                pending_finalizations,
            }) => self.begin_shutdown(pending_finalizations),
            Err(TryRecvError::Disconnected) => {
                self.begin_shutdown(Vec::new());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn begin_shutdown(&mut self, pending_finalizations: Vec<MediaJob>) {
        if self.shutdown_at.is_none() {
            self.shutdown_at = Some(Instant::now());
            self.shutdown_finalizations.extend(pending_finalizations);
        }
    }

    fn emit(&self, event: MediaEvent) {
        let _ = self.events.send(event);
    }

    fn run_job(&mut self, job: MediaJob) {
        let kind = job.kind();
        let outcome = match job {
            MediaJob::FinalizeRecording {
                draft,
                artifacts,
                facts,
            } => self.finalize_recording(&draft, &artifacts, facts),
            MediaJob::CreateClip {
                source,
                start_ms,
                end_ms,
            } => self.create_clip(&source, start_ms, end_ms),
        };
        match outcome {
            Ok(Some(entry)) => self.emit(MediaEvent::Completed {
                kind,
                entry: Box::new(entry),
            }),
            Ok(None) => self.emit(MediaEvent::Cancelled { kind }),
            Err(message) => self.emit(MediaEvent::Failed { kind, message }),
        }
    }

    fn finalize_recording(
        &mut self,
        draft: &RecordingDraft,
        artifacts: &CaptureArtifacts,
        facts: MediaFacts,
    ) -> Result<Option<LibraryEntry>, String> {
        let final_temp = self
            .job_file("final", "mp4")
            .map_err(|error| format!("final media temp: {error}"))?;

        let combined = match self.combine(artifacts, &final_temp)? {
            Some(actual_replay_ms) => CombinedMedia {
                temp_media: final_temp.clone(),
                actual_replay_ms,
                facts,
            },
            None => {
                let _ = fs::remove_file(&final_temp);
                return Ok(None);
            }
        };

        if fs::metadata(&combined.temp_media)
            .map(|meta| meta.len())
            .unwrap_or(0)
            == 0
        {
            let _ = fs::remove_file(&combined.temp_media);
            return Err("FFmpeg produced an empty recording".to_owned());
        }

        let entry = self
            .storage
            .finalize(draft, artifacts, &combined)
            .map_err(|error| {
                let _ = fs::remove_file(&combined.temp_media);
                format!("finalize: {error}")
            })?;
        tracing::info!(
            id = %draft.id,
            actual_replay_ms = combined.actual_replay_ms,
            "finalized recording"
        );
        Ok(Some(entry))
    }

    /// Measure the replay and concatenate it in front of the regular
    /// recording. Returns usable replay milliseconds, or `None` when
    /// cancelled. Any replay problem falls back to the regular recording alone.
    fn combine(
        &mut self,
        artifacts: &CaptureArtifacts,
        final_temp: &Path,
    ) -> Result<Option<u64>, String> {
        let Some(replay) = artifacts.replay.as_deref().filter(|path| path.exists()) else {
            return self.regular_only(artifacts, final_temp);
        };

        // GSR saves whole seconds from a keyframe, so the replay holds up to
        // one keyframe interval more than requested. Remuxing it to the null
        // sink yields the real lead-in the timeline is placed against.
        let actual_replay_ms = match self.run_ffmpeg(WorkKind::Finalize, measure_args(replay), None)
        {
            FfmpegOutcome::Done { out_time_ms } if out_time_ms > 0 => out_time_ms,
            FfmpegOutcome::Cancelled => return Ok(None),
            other => {
                if let FfmpegOutcome::Failed { message } = other {
                    tracing::warn!(%message, "replay measure failed; keeping the regular recording only");
                }
                return self.regular_only(artifacts, final_temp);
            }
        };

        let list_temp = self
            .job_file("concat", "txt")
            .map_err(|error| format!("concat list temp: {error}"))?;
        let list = format!(
            "file '{}'\nfile '{}'\n",
            escape_concat(replay),
            escape_concat(&artifacts.regular)
        );
        if let Err(error) = fs::write(&list_temp, list) {
            let _ = fs::remove_file(&list_temp);
            return self
                .regular_only(artifacts, final_temp)
                .map_err(|_| format!("write concat list: {error}"));
        }

        let concat = self.run_ffmpeg(
            WorkKind::Finalize,
            concat_args(&list_temp, final_temp),
            None,
        );
        let _ = fs::remove_file(&list_temp);

        match concat {
            FfmpegOutcome::Done { .. } => Ok(Some(actual_replay_ms)),
            FfmpegOutcome::Cancelled => Ok(None),
            FfmpegOutcome::Failed { message } => {
                tracing::warn!(%message, "replay concat failed; keeping the regular recording only");
                self.regular_only(artifacts, final_temp)
            }
        }
    }

    /// The regular recording alone, remuxed so the library only holds MP4.
    fn regular_only(
        &mut self,
        artifacts: &CaptureArtifacts,
        final_temp: &Path,
    ) -> Result<Option<u64>, String> {
        let args = copy_args(&[], &artifacts.regular, &[], final_temp);
        match self.run_ffmpeg(WorkKind::Finalize, args, None) {
            FfmpegOutcome::Done { .. } => Ok(Some(0)),
            FfmpegOutcome::Cancelled => Ok(None),
            FfmpegOutcome::Failed { message } => Err(format!("remux regular recording: {message}")),
        }
    }

    fn create_clip(
        &mut self,
        source: &LibraryEntry,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<Option<LibraryEntry>, String> {
        if end_ms <= start_ms || end_ms > source.duration_ms {
            return Err(format!(
                "clip range {start_ms}..{end_ms} ms is outside the {} ms source",
                source.duration_ms
            ));
        }
        if !source.media_path.exists() {
            return Err(format!(
                "clip source {} is missing",
                source.media_path.display()
            ));
        }

        let duration_ms = end_ms - start_ms;
        let created_at = now_unix_ms();
        let id = RecordingId::new();
        let stem = unique_stem(
            &id,
            created_at,
            &format!(
                "{} - Clipped at {}",
                sanitize_name(&source.title),
                format_local_stamp(created_at, self.config.utc_offset_minutes)
            ),
        );
        let (media_path, sidecar_path) = self
            .storage
            .claim_output(&stem)
            .map_err(|error| format!("clip output: {error}"))?;

        let temp = self
            .job_file("clip", "mp4")
            .map_err(|error| format!("clip temp: {error}"))?;
        let outcome = self.run_ffmpeg(
            WorkKind::Clip,
            clip_args(&source.media_path, start_ms, duration_ms, &temp),
            Some(duration_ms),
        );

        match outcome {
            FfmpegOutcome::Done { .. } => {}
            FfmpegOutcome::Cancelled => {
                let _ = fs::remove_file(&temp);
                let _ = fs::remove_file(&media_path);
                return Ok(None);
            }
            FfmpegOutcome::Failed { message } => {
                let _ = fs::remove_file(&temp);
                let _ = fs::remove_file(&media_path);
                return Err(message);
            }
        }

        let entry = LibraryEntry {
            id,
            media_path,
            sidecar_path,
            category: Category::Clip,
            flavor: source.flavor.clone(),
            title: source.title.clone(),
            start_unix_ms: created_at,
            duration_ms,
            outcome: source.outcome,
            // Clips are protected so eviction cannot reclaim deliberate work.
            protected: true,
            tag: None,
            activity_hash: source.activity_hash.clone(),
            player: source.player.clone(),
            combatants: source.combatants.clone(),
            details: ActivityDetails::Clip {
                source_recording: source.id.clone(),
                source_category: source.category.clone(),
                source_title: Some(source.title.clone()),
            },
            timeline: clip_timeline(&source.timeline, start_ms, end_ms),
            media: source.media.clone(),
        };

        // Pre-aggregated fights cannot be re-cut to an arbitrary range, so a
        // clip carries no meter.
        self.storage
            .write_new_entry(&entry, &MeterData::default(), &temp)
            .map_err(|error| {
                let _ = fs::remove_file(&temp);
                format!("write clip: {error}")
            })?;
        Ok(Some(entry))
    }

    /// Exclusively create a per-job file in the capture staging directory.
    fn job_file(&self, prefix: &str, extension: &str) -> io::Result<PathBuf> {
        let staging = self.storage.staging_dir();
        fs::create_dir_all(staging)?;
        let path = staging.join(format!("{prefix}-{}.{extension}", Uuid::new_v4()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(|_| path)
    }

    /// Spawn FFmpeg and poll it. Progress and stderr go to exclusively created
    /// per-job files (never pipes), removed afterwards; control is observed
    /// within one interval.
    fn run_ffmpeg(
        &mut self,
        kind: WorkKind,
        args: Vec<String>,
        total_ms: Option<u64>,
    ) -> FfmpegOutcome {
        let progress_path = self.job_file("progress", "txt");
        let log_path = self.job_file("ffmpeg", "log");
        let outcome = match (&progress_path, &log_path) {
            (Ok(progress_path), Ok(log_path)) => {
                self.spawn_and_poll(kind, &args, progress_path, log_path, total_ms)
            }
            (Err(error), _) | (_, Err(error)) => FfmpegOutcome::Failed {
                message: format!("FFmpeg job file: {error}"),
            },
        };
        for path in [progress_path, log_path].into_iter().flatten() {
            let _ = fs::remove_file(path);
        }
        outcome
    }

    fn spawn_and_poll(
        &mut self,
        kind: WorkKind,
        args: &[String],
        progress_path: &Path,
        log_path: &Path,
        total_ms: Option<u64>,
    ) -> FfmpegOutcome {
        let spawned = OpenOptions::new()
            .write(true)
            .open(log_path)
            .and_then(|log| {
                process::command(&self.config.ffmpeg)
                    // The GTK process constrains allocator arenas for its RSS
                    // gate; media tools must retain their own defaults.
                    .env_remove("MALLOC_ARENA_MAX")
                    .arg("-progress")
                    .arg(progress_path)
                    .arg("-nostats")
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(log))
                    .spawn()
            });
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return FfmpegOutcome::Failed {
                    message: format!("spawn FFmpeg: {error}"),
                };
            }
        };
        match self.poll_child(kind, &mut child, progress_path, total_ms) {
            FfmpegOutcome::Failed { message } => {
                let tail = process::read_log_tail(log_path);
                let tail = tail.trim();
                FfmpegOutcome::Failed {
                    message: if tail.is_empty() {
                        message
                    } else {
                        format!("{message}: {tail}")
                    },
                }
            }
            other => other,
        }
    }

    fn poll_child(
        &mut self,
        kind: WorkKind,
        child: &mut Child,
        progress_path: &Path,
        total_ms: Option<u64>,
    ) -> FfmpegOutcome {
        let mut progress = ProgressReader::new(progress_path);
        let mut out_time_ms = 0u64;
        let mut last_progress_emit = Instant::now() - PROGRESS_EMIT_INTERVAL;
        let grace = match kind {
            WorkKind::Finalize => self.config.finalize_grace,
            _ => Duration::ZERO,
        };
        let mut interrupt_at = self.shutdown_at.map(|shutdown_at| shutdown_at + grace);

        loop {
            if interrupt_at.is_some() {
                // Shutdown is one-shot: polling a dead channel would busy-spin
                // and must not keep pushing the deadline into the future.
                std::thread::sleep(self.config.poll_interval);
            } else {
                let shutdown = match self.control.recv_timeout(self.config.poll_interval) {
                    Ok(MediaControl::Shutdown {
                        pending_finalizations,
                    }) => Some(pending_finalizations),
                    Err(RecvTimeoutError::Disconnected) => Some(Vec::new()),
                    Err(RecvTimeoutError::Timeout) => None,
                };
                if let Some(pending_finalizations) = shutdown {
                    self.begin_shutdown(pending_finalizations);
                    interrupt_at = self.shutdown_at.map(|shutdown_at| shutdown_at + grace);
                }
            }

            if let Some(latest) = progress.read() {
                out_time_ms = latest;
                if last_progress_emit.elapsed() >= PROGRESS_EMIT_INTERVAL {
                    self.emit(MediaEvent::Progress(WorkProgress {
                        kind,
                        completed: latest,
                        total: total_ms,
                    }));
                    last_progress_emit = Instant::now();
                }
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(latest) = progress.read() {
                        out_time_ms = latest;
                    }
                    return if status.success() {
                        FfmpegOutcome::Done { out_time_ms }
                    } else if interrupt_at.is_some() {
                        FfmpegOutcome::Cancelled
                    } else {
                        FfmpegOutcome::Failed {
                            message: format!("FFmpeg exited with {status}"),
                        }
                    };
                }
                Ok(None) => {}
                Err(error) => {
                    return FfmpegOutcome::Failed {
                        message: format!("spawning FFmpeg: {error}"),
                    };
                }
            }

            if interrupt_at.is_some_and(|deadline| Instant::now() >= deadline) {
                // SIGINT, then kill if it does not go away.
                let _ = process::terminate(child, self.config.sigint_grace);
                return FfmpegOutcome::Cancelled;
            }
        }
    }
}

enum FfmpegOutcome {
    Done { out_time_ms: u64 },
    Failed { message: String },
    Cancelled,
}

/// Incremental reader for FFmpeg's `-progress` file: keeps a byte offset and a
/// partial-line buffer so only complete `key=value` lines are consumed.
struct ProgressReader {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl ProgressReader {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            offset: 0,
            partial: String::new(),
        }
    }

    /// Latest `out_time_us`/`out_time_ms` value, in milliseconds.
    fn read(&mut self) -> Option<u64> {
        let mut file = File::open(&self.path).ok()?;
        let size = file.metadata().ok()?.len();
        if size <= self.offset {
            return None;
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buffer = String::new();
        let read = file.read_to_string(&mut buffer).ok()?;
        self.offset += read as u64;
        self.partial.push_str(&buffer);

        let complete_to = self.partial.rfind('\n').map(|index| index + 1)?;
        let complete: String = self.partial.drain(..complete_to).collect();

        let mut latest = None;
        for line in complete.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "out_time_us" | "out_time_ms" => {
                    // Both keys have carried microseconds since 2017.
                    if let Ok(micros) = value.parse::<u64>() {
                        latest = Some(micros / 1000);
                    }
                }
                _ => {}
            }
        }
        latest
    }
}

/// Stream-copy `input` to `output`, with `before` as input options and
/// `after` as extra output options.
fn copy_args(before: &[&str], input: &Path, after: &[&str], output: &Path) -> Vec<String> {
    let input = input.to_string_lossy();
    let output = output.to_string_lossy();
    let mut args = vec!["-nostdin", "-hide_banner"];
    args.extend(before);
    args.extend(["-i", &input]);
    args.extend(after);
    args.extend(["-c:v", "copy", "-c:a", "copy"]);
    args.extend(["-avoid_negative_ts", "make_zero", "-y", &output]);
    args.into_iter().map(str::to_owned).collect()
}

/// Remux to the null sink purely to learn a file's real duration; the bundled
/// FFmpeg has no `ffprobe` and the null muxer is not built in.
fn measure_args(media: &Path) -> Vec<String> {
    vec![
        "-nostdin".to_owned(),
        "-hide_banner".to_owned(),
        "-i".to_owned(),
        media.to_string_lossy().into_owned(),
        "-c".to_owned(),
        "copy".to_owned(),
        "-y".to_owned(),
        "-f".to_owned(),
        "matroska".to_owned(),
        if cfg!(windows) { "NUL" } else { "/dev/null" }.to_owned(),
    ]
}

fn concat_args(list: &Path, output: &Path) -> Vec<String> {
    copy_args(&["-f", "concat", "-safe", "0"], list, &[], output)
}

/// Input-side seek plus output-side duration, with a pure stream copy.
fn clip_args(source: &Path, start_ms: u64, duration_ms: u64, output: &Path) -> Vec<String> {
    copy_args(
        &["-ss", &format_seconds(start_ms)],
        source,
        &[
            "-t",
            &format_seconds(duration_ms),
            "-movflags",
            "+faststart",
        ],
        output,
    )
}

fn clip_timeline(timeline: &[TimelineItem], start_ms: u64, end_ms: u64) -> Vec<TimelineItem> {
    let mut clipped = Vec::new();
    for item in timeline {
        let item_start = item.start_ms();
        let item_end = item.end_ms().unwrap_or(item_start);
        if item_end < start_ms || item_start > end_ms {
            continue;
        }
        match item.shape() {
            TimelineShape::Point => {
                if item_start < start_ms {
                    continue;
                }
                clipped.push(TimelineItem::point(
                    item.kind().clone(),
                    item_start - start_ms,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ));
            }
            TimelineShape::Span => {
                let span_start = item_start.max(start_ms) - start_ms;
                let span_end = item_end.min(end_ms) - start_ms;
                if let Ok(span) = TimelineItem::span(
                    item.kind().clone(),
                    span_start,
                    span_end,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ) {
                    clipped.push(span);
                }
            }
        }
    }
    clipped
}

fn format_seconds(milliseconds: u64) -> String {
    format_number(milliseconds as f64 / 1000.0)
}

/// Trim trailing zeros so whole seconds stay readable in argv and goldens.
fn format_number(value: f64) -> String {
    let text = format!("{value:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

fn escape_concat(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

/// `YYYY-MM-DD HH-MM-SS` in the supplied local offset.
fn format_local_stamp(epoch_ms: i64, utc_offset_minutes: i32) -> String {
    let local_seconds = epoch_ms.div_euclid(1000) + i64::from(utc_offset_minutes) * 60;
    let days = local_seconds.div_euclid(86_400);
    let seconds_of_day = local_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}-{:02}-{:02}",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc::{Sender, sync_channel};
    use std::thread;

    use crate::domain::{Codec, GameFlavor, MeterData, Outcome, TimelineKind};
    use crate::storage::{SIDECAR_SCHEMA_VERSION, test_root};

    fn fake_ffmpeg() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/native/bin/fake-ffmpeg.sh")
    }

    struct Harness {
        root: PathBuf,
        jobs: Sender<MediaJob>,
        control: std::sync::mpsc::SyncSender<MediaControl>,
        events: Receiver<MediaEvent>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            Self::with_finalize_grace(name, Duration::from_millis(0))
        }

        fn with_finalize_grace(name: &str, finalize_grace: Duration) -> Self {
            let root = test_root(&format!("media-{name}"));
            let storage = Storage::new(root.join("recordings with space"), root.join("capture"));
            storage.prepare().expect("prepare");
            fs::create_dir_all(root.join("capture/replay")).expect("replay dir");
            fs::create_dir_all(root.join("capture/regular")).expect("regular dir");

            let (jobs, jobs_rx) = std::sync::mpsc::channel();
            let (control, control_rx) = sync_channel(1);
            let (events_tx, events) = std::sync::mpsc::channel();
            let config = MediaConfig {
                ffmpeg: fake_ffmpeg(),
                utc_offset_minutes: 0,
                poll_interval: Duration::from_millis(20),
                finalize_grace,
                sigint_grace: Duration::from_millis(400),
            };
            let worker = thread::spawn(move || {
                MediaWorker::new(config, storage, jobs_rx, control_rx, events_tx).run();
            });
            Self {
                root,
                jobs,
                control,
                events,
                worker: Some(worker),
            }
        }

        fn library(&self) -> PathBuf {
            self.root.join("recordings with space")
        }

        fn staging(&self) -> PathBuf {
            self.root.join("capture/staging")
        }

        fn set_mode(&self, mode: &str) {
            fs::write(self.staging().join("fake-ffmpeg-mode"), mode).expect("mode");
        }

        fn argv(&self) -> Vec<String> {
            fs::read_to_string(self.staging().join("fake-ffmpeg-argv.txt"))
                .expect("argv")
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// Wait for the next non-progress event.
        fn outcome(&self) -> MediaEvent {
            loop {
                match self
                    .events
                    .recv_timeout(Duration::from_secs(20))
                    .expect("media event")
                {
                    MediaEvent::Progress(_) => continue,
                    other => return other,
                }
            }
        }

        fn progress(&self) -> Vec<WorkProgress> {
            self.events
                .try_iter()
                .filter_map(|event| match event {
                    MediaEvent::Progress(progress) => Some(progress),
                    _ => None,
                })
                .collect()
        }

        fn source(&self, name: &str, duration_ms: u64) -> LibraryEntry {
            let media_path = self.library().join(format!("{name}.mp4"));
            fs::write(&media_path, "source media").expect("source media");
            LibraryEntry {
                id: RecordingId::new(),
                media_path,
                sidecar_path: self.library().join(format!("{name}.json")),
                category: Category::Raids,
                flavor: GameFlavor::Retail,
                title: format!("{name} - Chrome King Gallywix [M] (Kill)"),
                start_unix_ms: 1_772_323_200_000,
                duration_ms,
                outcome: Outcome::Win,
                protected: false,
                tag: None,
                activity_hash: Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned()),
                player: None,
                combatants: Vec::new(),
                details: ActivityDetails::Raid {
                    zone_id: Some(2769),
                    zone_name: Some("Undermine".to_owned()),
                    encounter_id: Some(3009),
                    encounter_name: Some("Chrome King Gallywix".to_owned()),
                    difficulty_id: Some(16),
                    difficulty: Some("M".to_owned()),
                    pull: None,
                    boss_percent: Some(0),
                },
                timeline: vec![
                    TimelineItem::point(
                        TimelineKind::Death,
                        5_000,
                        Some("Early".to_owned()),
                        Some(Outcome::Loss),
                        None,
                    ),
                    TimelineItem::point(
                        TimelineKind::Death,
                        30_000,
                        Some("Inside".to_owned()),
                        Some(Outcome::Loss),
                        None,
                    ),
                    TimelineItem::span(TimelineKind::Encounter, 10_000, 40_000, None, None, None)
                        .expect("span"),
                ],
                media: MediaFacts {
                    fps: Some(60),
                    width: Some(1920),
                    height: Some(1080),
                    codec: Some(Codec::H264),
                    has_content: true,
                },
            }
        }

        fn shutdown_and_join(&mut self) {
            let _ = self.control.send(MediaControl::Shutdown {
                pending_finalizations: Vec::new(),
            });
            if let Some(worker) = self.worker.take() {
                worker.join().expect("join worker");
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            drop(std::mem::replace(
                &mut self.jobs,
                std::sync::mpsc::channel().0,
            ));
            if let Some(worker) = self.worker.take() {
                let _ = self.control.try_send(MediaControl::Shutdown {
                    pending_finalizations: Vec::new(),
                });
                let _ = worker.join();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn any_running(pattern: &str) -> bool {
        Command::new("pgrep")
            .arg("-f")
            .arg(pattern)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[test]
    fn clip_job_writes_a_clips_entry_for_the_selected_interval() {
        let mut harness = Harness::new("clip");
        let source = harness.source("Raid POV", 60_000);
        harness
            .jobs
            .send(MediaJob::CreateClip {
                source: Box::new(source.clone()),
                start_ms: 10_000,
                end_ms: 35_000,
            })
            .expect("send clip");

        let MediaEvent::Completed { kind, entry } = harness.outcome() else {
            panic!("clip did not complete");
        };
        assert_eq!(kind, WorkKind::Clip);
        assert_eq!(entry.category, Category::Clip);
        assert_eq!(entry.duration_ms, 25_000);
        assert!(entry.protected);
        assert_eq!(
            entry.details,
            ActivityDetails::Clip {
                source_recording: source.id.clone(),
                source_category: Category::Raids,
                source_title: Some(source.title.clone()),
            }
        );
        // The early death is outside the clip; the later one and the truncated
        // encounter span move to clip-relative offsets.
        assert_eq!(entry.timeline.len(), 2);
        assert_eq!(entry.timeline[0].start_ms(), 20_000);
        assert_eq!(entry.timeline[1].start_ms(), 0);
        assert_eq!(entry.timeline[1].end_ms(), Some(25_000));

        assert!(entry.media_path.exists());
        assert!(source.media_path.exists(), "clip source must be retained");
        let sidecar: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&entry.sidecar_path).expect("sidecar"))
                .expect("json");
        assert_eq!(sidecar["schema_version"], SIDECAR_SCHEMA_VERSION);
        assert!(
            entry.media_path.to_string_lossy().contains("Clipped at "),
            "{}",
            entry.media_path.display()
        );

        let argv = harness.argv();
        assert!(argv.contains(&"-progress".to_owned()) && argv.contains(&"-nostats".to_owned()));
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-ss" && pair[1] == "10")
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-t" && pair[1] == "25")
        );
        // Paths with spaces stay single arguments.
        assert!(argv.contains(&source.media_path.to_string_lossy().into_owned()));
        harness.shutdown_and_join();
        assert!(
            harness
                .staging()
                .read_dir()
                .expect("staging")
                .next()
                .is_some()
        );
    }

    fn finalize_job(harness: &Harness, with_replay: bool) -> MediaJob {
        let regular = harness.root.join("capture/regular/Video.mkv");
        fs::write(&regular, "regular bytes").expect("regular");
        let replay = with_replay.then(|| {
            let replay = harness.root.join("capture/replay/Replay.mkv");
            fs::write(&replay, "replay bytes").expect("replay");
            replay
        });
        MediaJob::FinalizeRecording {
            draft: Box::new(RecordingDraft {
                id: RecordingId::new(),
                category: Category::Raids,
                flavor: GameFlavor::Retail,
                started_at_ms: 1_772_323_200_000,
                overrun_ms: 15_000,
                details: ActivityDetails::Raid {
                    zone_id: Some(2769),
                    zone_name: Some("Undermine".to_owned()),
                    encounter_id: Some(3009),
                    encounter_name: Some("Chrome King Gallywix".to_owned()),
                    difficulty_id: Some(16),
                    difficulty: Some("M".to_owned()),
                    pull: None,
                    boss_percent: Some(0),
                },
                player: None,
                combatants: Vec::new(),
                timeline: vec![TimelineItem::point(
                    TimelineKind::Death,
                    20_000,
                    Some("Testone".to_owned()),
                    Some(Outcome::Loss),
                    None,
                )],
                outcome: Some(Outcome::Win),
                ended_at_ms: Some(1_772_323_270_000),
                duration_ms: Some(85_000),
                title: Some("Testone - Chrome King Gallywix [M] (Kill)".to_owned()),
                activity_hash: Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned()),
                meter: MeterData::default(),
            }),
            artifacts: CaptureArtifacts {
                replay,
                regular,
                regular_started_at_ms: 1_772_323_205_000,
                regular_stopped_at_ms: 1_772_323_275_000,
            },
            facts: MediaFacts {
                fps: Some(60),
                width: None,
                height: None,
                codec: Some(Codec::H264),
                has_content: true,
            },
        }
    }

    #[test]
    fn finalization_measures_and_concatenates_the_replay() {
        let mut harness = Harness::new("finalize");
        harness
            .jobs
            .send(finalize_job(&harness, true))
            .expect("send");

        let MediaEvent::Completed { kind, entry } = harness.outcome() else {
            panic!("finalize did not complete");
        };
        assert_eq!(kind, WorkKind::Finalize);
        // The fake reports three seconds of usable replay in front of a
        // seventy-second regular recording.
        assert_eq!(entry.duration_ms, 73_000);
        assert_eq!(entry.timeline[0].start_ms(), 18_000);
        assert!(entry.media_path.exists());

        // The concat used the demuxer list; its intermediates are gone.
        let argv = harness.argv();
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-f" && pair[1] == "concat")
        );
        let leftovers: Vec<PathBuf> = fs::read_dir(harness.staging())
            .expect("staging")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) != Some("txt"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        harness.shutdown_and_join();
    }

    #[test]
    fn shutdown_processes_finalization_carried_outside_the_full_job_channel() {
        let mut harness =
            Harness::with_finalize_grace("shutdown-pending-finalize", Duration::from_secs(5));
        let pending = finalize_job(&harness, true);

        harness
            .control
            .send(MediaControl::Shutdown {
                pending_finalizations: vec![pending],
            })
            .expect("shutdown");

        let MediaEvent::Completed { kind, entry } = harness.outcome() else {
            panic!("pending finalization did not complete during shutdown");
        };
        assert_eq!(kind, WorkKind::Finalize);
        assert!(entry.media_path.exists());
        harness.worker.take().expect("worker").join().expect("join");
    }

    #[test]
    fn a_failing_replay_measure_falls_back_to_the_regular_recording_alone() {
        let mut harness = Harness::new("finalize-fallback");
        harness.set_mode("fail-measure");
        harness
            .jobs
            .send(finalize_job(&harness, true))
            .expect("send");

        let MediaEvent::Completed { entry, .. } = harness.outcome() else {
            panic!("fallback did not complete");
        };
        // Zero usable replay: the media starts with the regular recording and
        // the marker keeps its activity-relative distance minus the lead-in.
        assert_eq!(entry.duration_ms, 70_000);
        assert_eq!(entry.timeline[0].start_ms(), 15_000);
        assert_regular_remuxed(&harness, &entry);
        harness.shutdown_and_join();
    }

    /// The last FFmpeg run remuxed the regular recording into the entry, so
    /// GSR's Matroska never lands in the library as-is.
    fn assert_regular_remuxed(harness: &Harness, entry: &LibraryEntry) {
        let regular = harness.root.join("capture/regular/Video.mkv");
        let argv = harness.argv();
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-i" && pair[1] == regular.to_string_lossy())
        );
        assert!(argv.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert_eq!(
            fs::read_to_string(&entry.media_path).expect("media"),
            "fake ffmpeg media"
        );
    }

    #[test]
    fn finalization_without_a_replay_remuxes_the_regular_recording() {
        let mut harness = Harness::new("finalize-regular-only");
        harness
            .jobs
            .send(finalize_job(&harness, false))
            .expect("send");

        let MediaEvent::Completed { entry, .. } = harness.outcome() else {
            panic!("regular-only finalize did not complete");
        };
        assert_eq!(entry.duration_ms, 70_000);
        assert_regular_remuxed(&harness, &entry);
        harness.shutdown_and_join();
    }

    #[test]
    fn shutdown_terminates_a_silent_child_and_leaves_no_process() {
        let mut harness = Harness::new("shutdown-silent");
        harness.set_mode("silent");
        let source = harness.source("Raid POV", 60_000);
        harness
            .jobs
            .send(MediaJob::CreateClip {
                source: Box::new(source),
                start_ms: 0,
                end_ms: 30_000,
            })
            .expect("send clip");

        // Let the child start before asking for shutdown.
        thread::sleep(Duration::from_millis(200));
        harness
            .control
            .send(MediaControl::Shutdown {
                pending_finalizations: Vec::new(),
            })
            .expect("shutdown");
        let event = harness.outcome();
        assert_eq!(
            event,
            MediaEvent::Cancelled {
                kind: WorkKind::Clip
            }
        );

        let pattern = harness.staging().to_string_lossy().into_owned();
        harness.worker.take().expect("worker").join().expect("join");
        assert!(!any_running(&pattern), "an FFmpeg child survived shutdown");
        // Nothing was published: only the untouched source media remains.
        let published: Vec<PathBuf> = fs::read_dir(harness.library())
            .expect("library")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(
            published[0].extension().and_then(|value| value.to_str()),
            Some("mp4")
        );
    }

    #[test]
    fn a_dropped_control_channel_still_interrupts_finalization() {
        // A nonzero grace proves the deadline is fixed once: if every
        // disconnected poll pushed it forward, the silent child would never
        // be interrupted and this test would time out.
        let mut harness =
            Harness::with_finalize_grace("shutdown-disconnect", Duration::from_millis(500));
        harness.set_mode("silent");
        harness
            .jobs
            .send(finalize_job(&harness, true))
            .expect("send");

        // Let the child start, then drop the sender (coordinator death). That
        // must behave like shutdown: the grace deadline is fixed once, not
        // pushed forward by every disconnected poll.
        thread::sleep(Duration::from_millis(200));
        drop(std::mem::replace(&mut harness.control, sync_channel(1).0));
        let event = harness.outcome();
        assert_eq!(
            event,
            MediaEvent::Cancelled {
                kind: WorkKind::Finalize
            }
        );

        let pattern = harness.staging().to_string_lossy().into_owned();
        harness.worker.take().expect("worker").join().expect("join");
        assert!(!any_running(&pattern), "an FFmpeg child survived shutdown");
    }

    #[test]
    fn progress_is_parsed_incrementally_while_ffmpeg_runs() {
        let mut harness = Harness::new("progress");
        harness.set_mode("chatty");
        let source = harness.source("Raid POV", 60_000);
        harness
            .jobs
            .send(MediaJob::CreateClip {
                source: Box::new(source),
                start_ms: 0,
                end_ms: 30_000,
            })
            .expect("send clip");

        thread::sleep(Duration::from_millis(200));
        assert!(
            harness
                .progress()
                .iter()
                .any(|progress| progress.completed > 0)
        );
        harness.shutdown_and_join();
    }

    #[test]
    fn a_failed_job_reports_the_ffmpeg_log_tail() {
        let mut harness = Harness::new("failure");
        harness.set_mode("fail");
        let source = harness.source("Raid POV", 60_000);
        harness
            .jobs
            .send(MediaJob::CreateClip {
                source: Box::new(source),
                start_ms: 0,
                end_ms: 30_000,
            })
            .expect("send clip");

        let MediaEvent::Failed { kind, message } = harness.outcome() else {
            panic!("clip did not fail");
        };
        assert_eq!(kind, WorkKind::Clip);
        assert!(message.contains("deliberate failure detail"), "{message}");
        assert!(message.len() <= 8 * 1024 + 200);
        // The claimed output name is released again.
        let published: Vec<PathBuf> = fs::read_dir(harness.library())
            .expect("library")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("mp4"))
            .collect();
        assert_eq!(published.len(), 1, "{published:?}");
        harness.shutdown_and_join();
    }

    #[test]
    fn local_stamps_and_clip_ranges_are_deterministic() {
        assert_eq!(
            format_local_stamp(1_772_323_200_000, 0),
            "2026-03-01 00-00-00"
        );
        assert_eq!(
            format_local_stamp(1_772_323_200_000, 120),
            "2026-03-01 02-00-00"
        );
        assert_eq!(format_local_stamp(0, -60), "1969-12-31 23-00-00");
        assert_eq!(format_seconds(1_500), "1.5");
        assert_eq!(format_seconds(30_000), "30");
    }
}
