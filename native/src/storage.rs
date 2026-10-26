// SPDX-License-Identifier: GPL-3.0-or-later

//! Filesystem-backed recording library.
//!
//! JSON sidecars next to the media are the only source of truth: there is no
//! database, thumbnail cache, or recursive crawl. `Storage` owns the configured
//! recording directory plus the capture root's `replay`/`regular`/`staging`
//! directories, and provides the scan, finalize, mutate, delete, evict, and
//! startup-sweep operations. FFmpeg work lives in `media_jobs`; this module
//! never spawns a process.
//!
//! `finalize` takes the already-combined media produced by the media worker, so
//! storage stays process-free. `update`, `delete`, and `enforce_limit` take
//! the scanned `LibraryEntry`, which already carries its
//! sidecar/media paths, so no second in-memory index is needed.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::activity::RecordingDraft;
use crate::domain::{
    ActivityDetails, BLOODLUST_DURATION_MS, Category, CombatantSummary, CorrelatedActivity,
    GameFlavor, LibraryEntry, MediaFacts, MeterData, MeterEntry, Outcome, PlayerSummary,
    RecordingId, RoundSummary, StorageLimit, TimelineItem, TimelineKind, TimelineShape,
};
use crate::meter::SAMPLE_INTERVAL_MS;
use crate::parser::days_from_civil;
use crate::recorder::CaptureArtifacts;

/// Schema version written into every native sidecar.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Multi-POV correlation tolerance on the activity start time.
const CORRELATION_TOLERANCE_MS: i64 = 60_000;

/// Directory (under the storage root) the startup sweep quarantines
/// unreferenced artifacts into.
pub const RECOVERY_DIR: &str = "Recovery";

const MEDIA_EXTENSION: &str = "mp4";
const SIDECAR_EXTENSION: &str = "json";

/// Media produced by the media worker for a finished capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CombinedMedia {
    /// Exclusively created temp file holding the final playable media.
    pub temp_media: PathBuf,
    /// Usable replay lead-in actually present at the front of `temp_media`.
    /// Zero for the regular-only fallback.
    pub actual_replay_ms: u64,
    pub facts: MediaFacts,
}

/// One sidecar that could not be turned into an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedEntry {
    pub sidecar_path: PathBuf,
    pub reason: String,
}

/// Result of a library scan. Holds no file handles or UI objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryIndex {
    /// Reverse chronological.
    pub entries: Arc<Vec<LibraryEntry>>,
    pub correlations: Arc<Vec<CorrelatedActivity>>,
    pub skipped: Vec<SkippedEntry>,
}

impl LibraryIndex {
    /// Fold in an entry the media worker just wrote, keeping the newest-first
    /// order and the correlation groups in step with a full scan. The entry
    /// carries its own sidecar start, so no file is read.
    pub fn upsert_entry(&mut self, entry: LibraryEntry) {
        let entries = Arc::make_mut(&mut self.entries);
        if let Some(existing) = entries
            .iter()
            .position(|candidate| candidate.id == entry.id)
        {
            entries.remove(existing);
        }
        let position = match entries.binary_search_by(|probe| entry_order(probe, &entry)) {
            Ok(position) | Err(position) => position,
        };
        entries.insert(position, entry);
        self.correlations = Arc::new(correlate(entries));
    }

    /// Drop the given ids (those actually present), rebuilding the correlation
    /// groups over what remains. A surviving viewpoint becomes its own group,
    /// the same result a rescan produces.
    pub fn remove_entries(&mut self, ids: &[RecordingId]) {
        let before = self.entries.len();
        let entries = Arc::make_mut(&mut self.entries);
        entries.retain(|entry| !ids.contains(&entry.id));
        if entries.len() != before {
            self.correlations = Arc::new(correlate(entries));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryUpdate {
    Protected(bool),
    /// An empty or whitespace-only tag clears it.
    Tag(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteResult {
    pub deleted: Vec<RecordingId>,
    pub failures: Vec<(RecordingId, String)>,
    /// A deletion removed the media but could not remove its sidecar, so the
    /// library no longer matches the directory. The caller has to rescan.
    pub partially_deleted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvictionResult {
    pub evicted: Vec<RecordingId>,
    pub remaining_bytes: u64,
    /// A deletion removed the media but could not remove its sidecar, so the
    /// library no longer matches the directory even though nothing was fully
    /// evicted. The caller has to rescan.
    pub partially_deleted: bool,
    /// Protected content alone exceeds a positive limit; nothing was deleted
    /// for it.
    pub protected_over_limit: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Quarantined files, by their new location under `Recovery/`.
    pub quarantined: Vec<PathBuf>,
    pub failures: Vec<String>,
}

pub struct Storage {
    root: PathBuf,
    replay_dir: PathBuf,
    regular_dir: PathBuf,
    staging_dir: PathBuf,
}

impl Storage {
    /// `root` is the configured recording directory; `capture_root` holds GSR's
    /// `replay`, `regular`, and `staging` subdirectories.
    pub fn new(root: impl Into<PathBuf>, capture_root: impl AsRef<Path>) -> Self {
        let capture_root = capture_root.as_ref();
        Self {
            root: root.into(),
            replay_dir: capture_root.join("replay"),
            regular_dir: capture_root.join("regular"),
            staging_dir: capture_root.join("staging"),
        }
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    /// Create the directories this module writes into.
    pub fn prepare(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.staging_dir)
    }

    // --- Scan ---

    /// Read every sidecar at the configured directory level. Unrelated files are
    /// ignored, unreadable sidecars are reported, and nothing is repaired.
    pub fn scan(&self) -> LibraryIndex {
        let mut index = LibraryIndex::default();
        let mut entries = Vec::new();

        // Media is discovered through its sidecar; the startup sweep deals
        // with anything unreferenced.
        for path in list_files(&self.root) {
            if path.extension().and_then(|value| value.to_str()) != Some(SIDECAR_EXTENSION) {
                continue;
            }
            match self.load_sidecar(&path) {
                Ok(entry) => entries.push(entry),
                Err(reason) => index.skipped.push(SkippedEntry {
                    sidecar_path: path,
                    reason,
                }),
            }
        }

        // The one library ordering: newest first, ties broken by media path.
        entries.sort_by(entry_order);
        index.correlations = Arc::new(correlate(&entries));
        index.entries = Arc::new(entries);
        index
    }

    fn load_sidecar(&self, path: &Path) -> Result<LibraryEntry, String> {
        // The meter is most of a sidecar and only the player needs it
        // (`load_meter`): compact native sidecars are read up to it, anything
        // else is read whole and the meter is skipped while parsing.
        let sidecar = match read_native_head(path) {
            Some(sidecar) => sidecar,
            None => {
                let text =
                    fs::read_to_string(path).map_err(|error| format!("unreadable: {error}"))?;
                // The probe only runs to tell a broken native sidecar from a
                // legacy one.
                match serde_json::from_str::<NativeSidecar<IgnoredAny>>(&text) {
                    Ok(sidecar) => sidecar,
                    Err(native_error) => {
                        let probe: SidecarProbe = serde_json::from_str(&text)
                            .map_err(|error| format!("invalid JSON: {error}"))?;
                        if probe.schema_version.is_some() {
                            return Err(format!("invalid native sidecar: {native_error}"));
                        }
                        return load_legacy_sidecar(path, &text);
                    }
                }
            }
        };
        if sidecar.schema_version > SIDECAR_SCHEMA_VERSION {
            return Err(format!(
                "sidecar schema version {} is newer than {SIDECAR_SCHEMA_VERSION}",
                sidecar.schema_version
            ));
        }
        let media_path = self.root.join(&sidecar.media_file);
        self.check_owned(&media_path)?;
        let has_content = media_has_content(&media_path)?;
        let mut entry = sidecar.into_entry(media_path, path.to_path_buf());
        entry.media.has_content = has_content;
        entry.validate().map_err(|error| error.to_string())?;
        Ok(entry)
    }

    // --- Finalization ---

    /// Turn a finished draft plus its combined media into a library entry:
    /// write the sidecar temp, rename media then sidecar, and only then remove
    /// the GSR intermediates. A crash between the renames leaves media the
    /// startup sweep quarantines; nothing playable is deleted.
    pub fn finalize(
        &self,
        draft: &RecordingDraft,
        artifacts: &CaptureArtifacts,
        media: &CombinedMedia,
    ) -> io::Result<LibraryEntry> {
        let media_start_ms = artifacts.regular_started_at_ms - media.actual_replay_ms as i64;
        let regular_ms = artifacts
            .regular_stopped_at_ms
            .saturating_sub(artifacts.regular_started_at_ms)
            .max(0) as u64;
        let duration_ms = media.actual_replay_ms + regular_ms;

        let title = draft
            .title
            .clone()
            .unwrap_or_else(|| default_title(&draft.category));
        let stem = unique_stem(&draft.id, draft.started_at_ms, &title);
        let (media_path, sidecar_path) = self.claim_output(&stem)?;

        let entry = LibraryEntry {
            id: draft.id.clone(),
            media_path,
            sidecar_path,
            category: draft.category.clone(),
            flavor: recorded_flavor(&draft.flavor),
            title,
            start_unix_ms: draft.started_at_ms,
            duration_ms,
            outcome: draft.outcome.unwrap_or(Outcome::Unknown),
            protected: false,
            tag: None,
            activity_hash: draft.activity_hash.clone(),
            player: draft.player.clone(),
            combatants: draft.combatants.clone(),
            details: draft.details.clone(),
            timeline: shift_timeline(
                &draft.timeline,
                draft.started_at_ms,
                media_start_ms,
                duration_ms,
            ),
            media: media.facts.clone(),
        };
        let meter = shift_meter(
            &draft.meter,
            draft.started_at_ms,
            media_start_ms,
            duration_ms,
        );

        self.write_new_entry(&entry, &meter, &media.temp_media)?;

        // Both final names exist: the intermediates are now safe to remove.
        if let Some(replay) = artifacts.replay.as_deref() {
            let _ = fs::remove_file(replay);
        }
        let _ = fs::remove_file(&artifacts.regular);

        Ok(entry)
    }

    /// Claim `<stem>.mp4` exclusively (adding a numeric suffix on collision) and
    /// return it with its sidecar path. Identifiers, not titles, provide the
    /// uniqueness; the suffix only guards an exact-name collision.
    pub fn claim_output(&self, stem: &str) -> io::Result<(PathBuf, PathBuf)> {
        for attempt in 0..100u32 {
            let candidate = if attempt == 0 {
                stem.to_owned()
            } else {
                format!("{stem} ({attempt})")
            };
            let media_path = self.root.join(format!("{candidate}.{MEDIA_EXTENSION}"));
            let sidecar_path = self.root.join(format!("{candidate}.{SIDECAR_EXTENSION}"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&media_path)
            {
                Ok(_) => return Ok((media_path, sidecar_path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free output name",
        ))
    }

    /// Write the sidecar temp, move the media into place, then rename the
    /// sidecar. `source_media` is consumed.
    pub fn write_new_entry(
        &self,
        entry: &LibraryEntry,
        meter: &MeterData,
        source_media: &Path,
    ) -> io::Result<()> {
        entry
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let sidecar_temp = temp_sibling(&entry.sidecar_path);
        let json = NativeSidecar::from_entry(entry, &self.root, meter).to_json()?;
        write_atomic(&sidecar_temp, None, json.as_bytes())?;
        move_file(source_media, &entry.media_path)?;
        fs::rename(&sidecar_temp, &entry.sidecar_path)
    }

    // --- Mutation and deletion ---

    /// Rewrite only the sidecar, atomically. A legacy sidecar keeps its original
    /// schema and unknown fields: only the `protected`/`tag` keys are patched so
    /// the legacy Electron app can still read it.
    pub fn update(&self, entry: &LibraryEntry, change: &EntryUpdate) -> io::Result<LibraryEntry> {
        self.check_owned(&entry.sidecar_path)
            .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
        let text = fs::read_to_string(&entry.sidecar_path)?;

        let mut updated = entry.clone();
        match change {
            EntryUpdate::Protected(protected) => updated.protected = *protected,
            EntryUpdate::Tag(tag) => {
                updated.tag = if tag.trim().is_empty() {
                    None
                } else {
                    Some(tag.clone())
                };
            }
        }

        // The entry does not hold the meter, so patch the document on disk
        // rather than rebuilding it from the entry. The meter is copied
        // through as raw JSON; only a sidecar without one takes the probe.
        let temp = temp_sibling(&entry.sidecar_path);
        if let Ok(mut sidecar) = serde_json::from_str::<NativeSidecar<Option<&RawValue>>>(&text)
            && sidecar.meter.is_some()
        {
            sidecar.protected = updated.protected;
            sidecar.tag = updated.tag.clone();
            write_atomic(
                &temp,
                Some(&entry.sidecar_path),
                sidecar.to_json()?.as_bytes(),
            )?;
            return Ok(updated);
        }
        let probe: SidecarProbe = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

        let json = if probe.schema_version.is_some() {
            let mut sidecar: NativeSidecar = serde_json::from_str(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            sidecar.protected = updated.protected;
            sidecar.tag = updated.tag.clone();
            sidecar.to_json()?
        } else {
            // The sole sanctioned untyped escape hatch; it never enters the
            // domain model.
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
            let Value::Object(mut object) = value else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy sidecar is not a JSON object",
                ));
            };
            match change {
                EntryUpdate::Protected(protected) => {
                    object.insert("protected".to_owned(), Value::Bool(*protected));
                }
                EntryUpdate::Tag(_) => match &updated.tag {
                    Some(tag) => {
                        object.insert("tag".to_owned(), Value::String(tag.clone()));
                    }
                    None => {
                        object.remove("tag");
                    }
                },
            }
            pretty_json(&Value::Object(object))?
        };

        write_atomic(&temp, Some(&entry.sidecar_path), json.as_bytes())?;
        Ok(updated)
    }

    /// Remove media plus sidecar for each entry, reporting per-entry failures.
    /// Symlinks and paths outside the root are refused.
    pub fn delete(&self, entries: &[LibraryEntry]) -> DeleteResult {
        let mut result = DeleteResult::default();
        for entry in entries {
            match self.delete_one(entry) {
                Ok(()) => result.deleted.push(entry.id.clone()),
                Err(error) => {
                    // Same rule as eviction: only a sidecar-stage failure
                    // leaves the media gone while its sidecar remains.
                    if error.starts_with("sidecar:") {
                        result.partially_deleted = true;
                    }
                    result.failures.push((entry.id.clone(), error));
                }
            }
        }
        result
    }

    fn delete_one(&self, entry: &LibraryEntry) -> Result<(), String> {
        self.check_owned(&entry.media_path)?;
        self.check_owned(&entry.sidecar_path)?;
        remove_media(&entry.media_path).map_err(|error| format!("media: {error}"))?;
        match fs::remove_file(&entry.sidecar_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("sidecar: {error}")),
        }
    }

    fn check_owned(&self, path: &Path) -> Result<(), String> {
        // The library is deliberately flat. Requiring a direct child closes
        // the intermediate-directory symlink race: a leaf swapped to a
        // symlink is itself unlinked by deletion, never followed.
        if path.parent() != Some(self.root.as_path()) {
            return Err(format!(
                "{} is not a direct child of the storage root",
                path.display()
            ));
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("{} is a symlink", path.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }

    // --- Storage limit ---

    /// Evict the oldest unprotected recordings until the library fits the limit.
    /// Unlimited returns without eviction; unrecognized files are never touched.
    pub fn enforce_limit(&self, limit: StorageLimit, entries: &[LibraryEntry]) -> EvictionResult {
        let mut result = EvictionResult::default();
        let usage = |entries: &[LibraryEntry]| -> u64 {
            entries
                .iter()
                .map(|entry| self.entry_size(entry))
                .fold(0u64, u64::saturating_add)
        };

        let StorageLimit::Gib(gib) = limit else {
            result.remaining_bytes = usage(entries);
            return result;
        };
        let Some(limit_bytes) = gib.get().checked_mul(1024 * 1024 * 1024) else {
            result.remaining_bytes = usage(entries);
            return result;
        };

        let mut sized: Vec<(&LibraryEntry, u64)> = entries
            .iter()
            .map(|entry| (entry, self.entry_size(entry)))
            .collect();
        let mut used: u64 = sized
            .iter()
            .map(|(_, size)| *size)
            .fold(0u64, u64::saturating_add);
        let protected_bytes: u64 = sized
            .iter()
            .filter(|(entry, _)| entry.protected)
            .map(|(_, size)| *size)
            .fold(0u64, u64::saturating_add);
        result.protected_over_limit = protected_bytes > limit_bytes;

        // Oldest first; the tie-break keeps the order deterministic.
        sized.sort_by(|left, right| {
            left.0
                .start_unix_ms
                .cmp(&right.0.start_unix_ms)
                .then_with(|| left.0.media_path.cmp(&right.0.media_path))
        });

        for (entry, size) in sized {
            if used <= limit_bytes {
                break;
            }
            if entry.protected {
                continue;
            }
            match self.delete_one(entry) {
                Ok(()) => {
                    used = used.saturating_sub(size);
                    result.evicted.push(entry.id.clone());
                }
                Err(error) => {
                    // `delete_one` unlinks the media before the sidecar and
                    // tags which stage failed. Only a sidecar failure means
                    // the media is actually gone; a media failure (already
                    // missing, not owned, a symlink) freed nothing, and
                    // probing `exists()` cannot tell those apart. Credit the
                    // space for the real case rather than evicting more, and
                    // make the caller rescan.
                    if error.starts_with("sidecar:") {
                        used = used.saturating_sub(size);
                        result.partially_deleted = true;
                    }
                }
            }
        }

        result.remaining_bytes = used;
        result
    }

    fn entry_size(&self, entry: &LibraryEntry) -> u64 {
        let media = fs::metadata(&entry.media_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let sidecar = fs::metadata(&entry.sidecar_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        media.saturating_add(sidecar)
    }

    // --- Recovery sweeps ---

    /// Quarantine every media/GSR/`.tmp` artifact in the storage, replay,
    /// regular, and staging directories that no sidecar references. `index` is
    /// a fresh scan of this storage: its entries name their media, and only
    /// the sidecars it skipped are read again. Runs at startup, before capture
    /// is armed. Nothing is deleted, repaired, or claimed by name/time
    /// proximity.
    pub fn sweep_orphans(&self, index: &LibraryIndex) -> RecoveryReport {
        self.sweep(Some(&self.referenced_media(index)))
    }

    /// Quarantine everything in the replay, regular, and staging directories
    /// but leave the storage folder alone. The caller guarantees no capture or
    /// media job still owns a file there.
    pub fn sweep_capture_dirs(&self) -> RecoveryReport {
        self.sweep(None)
    }

    /// Quarantine one capture's GSR intermediates, nothing else.
    pub fn quarantine_capture(&self, artifacts: &CaptureArtifacts, reason: &str) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let recovery_dir = self.root.join(RECOVERY_DIR);
        for path in artifacts.replay.iter().chain([&artifacts.regular]) {
            match self.quarantine(&recovery_dir, path, reason) {
                Ok(moved) => report.quarantined.push(moved),
                Err(error) => report.failures.push(format!("{}: {error}", path.display())),
            }
        }
        report
    }

    /// Sweep the capture directories, plus the storage folder when the media
    /// its sidecars reference is known.
    fn sweep(&self, referenced: Option<&HashSet<PathBuf>>) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        let recovery_dir = self.root.join(RECOVERY_DIR);

        let mut directories = Vec::new();
        if referenced.is_some() {
            directories.push((
                self.root.clone(),
                "unreferenced media or interrupted write in the storage folder",
            ));
        }
        directories.extend([
            (self.replay_dir.clone(), "replay artifact with no recording"),
            (
                self.regular_dir.clone(),
                "regular artifact with no recording",
            ),
            (self.staging_dir.clone(), "media job intermediate"),
        ]);

        for (directory, reason) in directories {
            for path in list_files(&directory) {
                if directory == self.root {
                    let extension = path.extension().and_then(|value| value.to_str());
                    let sweepable = matches!(extension, Some(MEDIA_EXTENSION) | Some("tmp"));
                    if !sweepable || referenced.is_some_and(|media| media.contains(&path)) {
                        continue;
                    }
                }
                match self.quarantine(&recovery_dir, &path, reason) {
                    Ok(moved) => report.quarantined.push(moved),
                    Err(error) => report.failures.push(format!("{}: {error}", path.display())),
                }
            }
        }
        report
    }

    fn referenced_media(&self, index: &LibraryIndex) -> HashSet<PathBuf> {
        let mut referenced: HashSet<PathBuf> = index
            .entries
            .iter()
            .map(|entry| entry.media_path.clone())
            .collect();
        for skipped in &index.skipped {
            let Ok(text) = fs::read_to_string(&skipped.sidecar_path) else {
                continue;
            };
            // A sidecar the scanner rejects can still own its media, so the
            // reference is probed without materializing the document.
            let Ok(probe) = serde_json::from_str::<SidecarProbe>(&text) else {
                continue;
            };
            match probe.media_file.as_deref() {
                Some(media_file) => {
                    referenced.insert(self.root.join(media_file));
                }
                None => {
                    referenced.insert(skipped.sidecar_path.with_extension(MEDIA_EXTENSION));
                }
            }
        }
        referenced
    }

    fn quarantine(&self, recovery_dir: &Path, path: &Path, reason: &str) -> io::Result<PathBuf> {
        fs::create_dir_all(recovery_dir)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_owned());
        let mut destination = recovery_dir.join(&name);
        let mut attempt = 1;
        while destination.exists() {
            destination = recovery_dir.join(format!("{attempt}-{name}"));
            attempt += 1;
        }
        move_file(path, &destination)?;
        let mut reason_file = File::create(reason_path(&destination))?;
        writeln!(reason_file, "{reason}\noriginal path: {}", path.display())?;
        Ok(destination)
    }
}

/// `to_json` writes compact JSON in field order with the meter last, so a
/// native sidecar's library fields are everything before the first
/// `,"meter":`. Reading stops there; `None` (legacy, pretty-printed, or a head
/// that does not parse) means the caller reads the whole file.
fn read_native_head(path: &Path) -> Option<NativeSidecar<IgnoredAny>> {
    const MARKER: &[u8] = b",\"meter\":";
    let mut file = File::open(path).ok()?;
    let mut head = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).ok().filter(|read| *read > 0)?;
        let from = head.len().saturating_sub(MARKER.len() - 1);
        head.extend_from_slice(&chunk[..read]);
        if !head.starts_with(b"{\"schema_version\":") {
            return None;
        }
        if let Some(found) = head[from..].windows(MARKER.len()).position(|w| w == MARKER) {
            head.truncate(from + found);
            head.push(b'}');
            return serde_json::from_slice(&head).ok();
        }
    }
}

/// Regular files directly in `directory`, sorted so the order does not depend
/// on directory iteration. A missing directory has none.
fn list_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

fn load_legacy_sidecar(path: &Path, text: &str) -> Result<LibraryEntry, String> {
    let legacy: LegacySidecar =
        serde_json::from_str(text).map_err(|error| format!("invalid legacy sidecar: {error}"))?;
    let media_path = path.with_extension(MEDIA_EXTENSION);
    let has_content = media_has_content(&media_path)?;
    // Legacy sidecars without a recorded start fall back to the media
    // mtime, which two POVs of one activity rarely share.
    let mtime_ms = file_modified_ms(&media_path).unwrap_or(0);
    let mut entry = legacy.into_entry(media_path, path.to_path_buf(), mtime_ms)?;
    entry.media.has_content = has_content;
    Ok(entry)
}

/// Classification probe: whether a non-null `schema_version` is present (native,
/// whatever its value) and which media file the sidecar names. Unknown fields,
/// the meter among them, are skipped instead of materialized.
#[derive(Deserialize)]
struct SidecarProbe {
    schema_version: Option<IgnoredAny>,
    media_file: Option<String>,
}

// --- Native sidecar ---

/// `M` is the meter payload: `MeterData` to read or write it, `IgnoredAny`
/// for the library scan, a reference when writing a meter held elsewhere,
/// `Option<&RawValue>` to patch a sidecar without parsing its meter.
#[derive(Debug, Serialize, Deserialize)]
struct NativeSidecar<M = MeterData> {
    schema_version: u32,
    /// Media file name relative to the storage root.
    media_file: String,
    id: RecordingId,
    category: Category,
    flavor: GameFlavor,
    title: String,
    start_unix_ms: i64,
    duration_ms: u64,
    outcome: Outcome,
    protected: bool,
    tag: Option<String>,
    activity_hash: Option<String>,
    player: Option<PlayerSummary>,
    combatants: Vec<CombatantSummary>,
    details: ActivityDetails,
    timeline: Vec<TimelineItem>,
    media: MediaFacts,
    /// Absent in sidecars written before the damage meter shipped.
    #[serde(default)]
    meter: M,
}

impl<M> NativeSidecar<M> {
    fn from_entry(entry: &LibraryEntry, root: &Path, meter: M) -> Self {
        let media_file = entry
            .media_path
            .strip_prefix(root)
            .unwrap_or(&entry.media_path)
            .to_string_lossy()
            .into_owned();
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            media_file,
            id: entry.id.clone(),
            category: entry.category.clone(),
            flavor: entry.flavor.clone(),
            title: entry.title.clone(),
            start_unix_ms: entry.start_unix_ms,
            duration_ms: entry.duration_ms,
            outcome: entry.outcome,
            protected: entry.protected,
            tag: entry.tag.clone(),
            activity_hash: entry.activity_hash.clone(),
            player: entry.player.clone(),
            combatants: entry.combatants.clone(),
            details: entry.details.clone(),
            timeline: entry.timeline.clone(),
            media: entry.media.clone(),
            meter,
        }
    }

    fn into_entry(self, media_path: PathBuf, sidecar_path: PathBuf) -> LibraryEntry {
        LibraryEntry {
            id: self.id,
            media_path,
            sidecar_path,
            category: self.category,
            flavor: self.flavor,
            title: self.title,
            start_unix_ms: self.start_unix_ms,
            duration_ms: self.duration_ms,
            outcome: self.outcome,
            protected: self.protected,
            tag: self.tag,
            activity_hash: self.activity_hash,
            player: self.player,
            combatants: self.combatants,
            details: self.details,
            timeline: self.timeline,
            media: self.media,
        }
    }
}

impl<M: Serialize> NativeSidecar<M> {
    /// Compact: meter payloads make pretty-printed sidecars several times
    /// larger and slower to parse, and every reader is format-agnostic.
    fn to_json(&self) -> io::Result<String> {
        serde_json::to_string(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

/// Read one sidecar's damage meter. The library index never holds meters
/// (they are most of a sidecar's size), so the player loads the selected one
/// on demand. Legacy sidecars and native ones written before the meter
/// shipped have none.
pub fn load_meter(sidecar_path: &Path) -> io::Result<MeterData> {
    #[derive(Deserialize)]
    struct MeterOnly {
        #[serde(default)]
        meter: MeterData,
    }
    let text = fs::read_to_string(sidecar_path)?;
    serde_json::from_str::<MeterOnly>(&text)
        .map(|sidecar| sidecar.meter)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

// --- Legacy sidecar ---

/// The real legacy JSON written by the Electron application. Private to
/// storage: it is converted to the clean model and never exposed. Cloud-only
/// fields are simply not read.
#[derive(Debug, Deserialize)]
struct LegacySidecar {
    #[serde(default)]
    category: String,
    #[serde(rename = "parentCategory")]
    parent_category: Option<String>,
    #[serde(default)]
    duration: f64,
    start: Option<f64>,
    #[serde(rename = "clippedAt")]
    clipped_at: Option<f64>,
    #[serde(default)]
    result: bool,
    flavour: Option<String>,
    #[serde(rename = "zoneID")]
    zone_id: Option<u32>,
    #[serde(rename = "zoneName")]
    zone_name: Option<String>,
    #[serde(rename = "encounterID")]
    encounter_id: Option<u32>,
    #[serde(rename = "encounterName")]
    encounter_name: Option<String>,
    #[serde(rename = "difficultyID")]
    difficulty_id: Option<u32>,
    difficulty: Option<String>,
    player: Option<LegacyCombatant>,
    #[serde(rename = "teamMMR")]
    team_mmr: Option<u32>,
    #[serde(default)]
    deaths: Vec<LegacyDeath>,
    #[serde(rename = "upgradeLevel")]
    upgrade_level: Option<u8>,
    #[serde(rename = "mapID")]
    map_id: Option<u32>,
    #[serde(rename = "challengeModeTimeline", default)]
    challenge_mode_timeline: Vec<LegacySegment>,
    #[serde(rename = "bloodlustTimeline", default)]
    bloodlust_timeline: Vec<LegacyBloodlust>,
    #[serde(rename = "soloShuffleTimeline", default)]
    solo_shuffle_timeline: Vec<LegacyRound>,
    /// Pre-cloud keystone level.
    level: Option<u32>,
    #[serde(rename = "keystoneLevel")]
    keystone_level: Option<u32>,
    #[serde(default)]
    protected: bool,
    #[serde(rename = "soloShuffleRoundsWon")]
    solo_shuffle_rounds_won: Option<u8>,
    #[serde(rename = "soloShuffleRoundsPlayed")]
    solo_shuffle_rounds_played: Option<u8>,
    #[serde(default)]
    combatants: Vec<LegacyCombatant>,
    #[serde(default)]
    affixes: Vec<u32>,
    tag: Option<String>,
    #[serde(rename = "uniqueHash")]
    unique_hash: Option<String>,
    #[serde(rename = "bossPercent")]
    boss_percent: Option<u8>,
    /// Recorder FPS; present on newer sidecars only.
    fps: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct LegacyCombatant {
    #[serde(rename = "_GUID")]
    guid: Option<String>,
    #[serde(rename = "_teamID")]
    team_id: Option<i32>,
    #[serde(rename = "_specID")]
    spec_id: Option<u16>,
    #[serde(rename = "_name")]
    name: Option<String>,
    #[serde(rename = "_realm")]
    realm: Option<String>,
    #[serde(rename = "_region")]
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyDeath {
    #[serde(default)]
    name: String,
    /// Seconds from the activity start.
    #[serde(default)]
    timestamp: f64,
    #[serde(default)]
    friendly: bool,
}

#[derive(Debug, Deserialize)]
struct LegacySegment {
    #[serde(rename = "segmentType")]
    segment_type: Option<String>,
    #[serde(rename = "logStart")]
    log_start: Option<String>,
    #[serde(rename = "logEnd")]
    log_end: Option<String>,
    /// Seconds from the activity start.
    timestamp: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LegacyRound {
    #[serde(default)]
    round: u32,
    /// Seconds from the activity start.
    #[serde(default)]
    timestamp: f64,
    #[serde(default)]
    result: bool,
    duration: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct LegacyBloodlust {
    /// Milliseconds since Unix epoch in the legacy sidecar, converted to a
    /// relative offset before the legacy sidecar enters the domain.
    #[serde(rename = "timestampMs")]
    timestamp_ms: i64,
    #[serde(rename = "spellName")]
    spell_name: String,
}

impl LegacySidecar {
    fn into_entry(
        self,
        media_path: PathBuf,
        sidecar_path: PathBuf,
        mtime_ms: i64,
    ) -> Result<LibraryEntry, String> {
        if self.category.trim().is_empty() {
            return Err("missing category".to_owned());
        }
        if !self.duration.is_finite() || self.duration < 0.0 {
            return Err(format!("invalid duration {}", self.duration));
        }
        // The Electron category set is closed: an unrecognized string means a
        // junk file, not a category the app should carry around.
        let category = legacy_category(&self.category)
            .ok_or_else(|| format!("unknown legacy category {:?}", self.category))?;
        // Clips are parented by category; an unknown or absent parent is
        // rejected instead of defaulting.
        let parent = match category {
            Category::Clip => match self.parent_category.as_deref().map(legacy_category) {
                Some(Some(parent)) => parent,
                Some(None) => {
                    return Err(format!(
                        "unknown legacy parent category {:?}",
                        self.parent_category.as_deref().unwrap_or_default()
                    ));
                }
                None => return Err("legacy clip without a parent category".to_owned()),
            },
            _ => category.clone(),
        };
        let duration_ms = (self.duration * 1000.0).round() as u64;
        let start_unix_ms = self
            .clipped_at
            .or(self.start)
            .filter(|value| value.is_finite())
            .map(|value| value as i64)
            .unwrap_or(mtime_ms);
        let outcome = legacy_outcome(&parent, self.result);
        let timeline = self.legacy_timeline(duration_ms);
        let media_name = media_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let details = self.legacy_details(&category, parent, &media_name);
        let title = match legacy_title(
            &self.encounter_name,
            &self.legacy_place_name(),
            self.player.as_ref(),
        ) {
            title if title.is_empty() => default_title(&category),
            title => title,
        };

        let entry = LibraryEntry {
            id: RecordingId::from_media_name(&media_name),
            media_path,
            sidecar_path,
            category,
            flavor: legacy_flavor(self.flavour.as_deref()),
            title,
            start_unix_ms,
            duration_ms,
            outcome,
            protected: self.protected,
            tag: self.tag.filter(|tag| !tag.trim().is_empty()),
            activity_hash: self.unique_hash,
            player: self.player.as_ref().and_then(legacy_player),
            combatants: self.combatants.iter().map(legacy_combatant).collect(),
            details,
            timeline,
            media: MediaFacts {
                fps: self.fps,
                width: None,
                height: None,
                codec: None,
                has_content: true,
            },
        };
        entry.validate().map_err(|error| error.to_string())?;
        Ok(entry)
    }

    fn legacy_timeline(&self, duration_ms: u64) -> Vec<TimelineItem> {
        let mut items: Vec<TimelineItem> = Vec::new();
        for death in &self.deaths {
            let start_ms = seconds_to_ms(death.timestamp);
            if start_ms > duration_ms {
                continue;
            }
            items.push(TimelineItem::point(
                TimelineKind::Death,
                start_ms,
                Some(death.name.clone()),
                Some(if death.friendly {
                    Outcome::Loss
                } else {
                    Outcome::Win
                }),
                None,
            ));
        }

        for segment in &self.challenge_mode_timeline {
            let Some(timestamp) = segment.timestamp else {
                continue;
            };
            let start_ms = seconds_to_ms(timestamp);
            if start_ms > duration_ms {
                continue;
            }
            // A legacy segment's length is the difference of its ISO log stamps.
            let length_ms = match (
                segment.log_start.as_deref().and_then(iso_epoch_ms),
                segment.log_end.as_deref().and_then(iso_epoch_ms),
            ) {
                (Some(from), Some(to)) => to.saturating_sub(from).max(0) as u64,
                _ => 0,
            };
            let kind = match segment.segment_type.as_deref() {
                Some("Boss") => TimelineKind::Encounter,
                Some("Trash") => TimelineKind::Trash,
                Some(other) => TimelineKind::Unknown(other.to_owned()),
                None => TimelineKind::Unknown(String::new()),
            };
            let end_ms = start_ms.saturating_add(length_ms).min(duration_ms);
            if let Ok(item) = TimelineItem::span(kind, start_ms, end_ms, None, None, None) {
                items.push(item);
            }
        }

        if let Some(activity_start_ms) = self.start.filter(|value| value.is_finite()) {
            let activity_start_ms = activity_start_ms as i64;
            for cast in &self.bloodlust_timeline {
                let start_ms = cast.timestamp_ms.saturating_sub(activity_start_ms).max(0) as u64;
                if start_ms > duration_ms {
                    continue;
                }
                let end_ms = start_ms
                    .saturating_add(BLOODLUST_DURATION_MS)
                    .min(duration_ms);
                if let Ok(item) = TimelineItem::span(
                    TimelineKind::Bloodlust,
                    start_ms,
                    end_ms,
                    Some(cast.spell_name.clone()),
                    None,
                    None,
                ) {
                    items.push(item);
                }
            }
        }

        for round in &self.solo_shuffle_timeline {
            let start_ms = seconds_to_ms(round.timestamp);
            if start_ms > duration_ms {
                continue;
            }
            let label = Some(format!("Round {}", round.round));
            let outcome = Some(if round.result {
                Outcome::Win
            } else {
                Outcome::Loss
            });
            let item = match round.duration {
                Some(seconds) => TimelineItem::span(
                    TimelineKind::Round,
                    start_ms,
                    start_ms
                        .saturating_add(seconds_to_ms(seconds))
                        .min(duration_ms),
                    label,
                    outcome,
                    None,
                )
                .ok(),
                None => Some(TimelineItem::point(
                    TimelineKind::Round,
                    start_ms,
                    label,
                    outcome,
                    None,
                )),
            };
            items.extend(item);
        }

        items.sort_by_key(TimelineItem::start_ms);
        items
    }

    /// Legacy sidecars usually store only `zoneID`/`mapID`; resolve the display
    /// name from the instance table when the sidecar carries no name.
    fn legacy_place_name(&self) -> Option<String> {
        self.zone_name.clone().or_else(|| {
            crate::activity::instance_name(
                &legacy_flavor(self.flavour.as_deref()),
                self.zone_id.unwrap_or(0),
                self.map_id.unwrap_or(0),
            )
        })
    }

    fn legacy_details(
        &self,
        category: &Category,
        parent: Category,
        media_name: &str,
    ) -> ActivityDetails {
        match category {
            Category::Clip => ActivityDetails::Clip {
                source_recording: legacy_clip_source_id(media_name),
                source_category: parent,
                source_title: self
                    .encounter_name
                    .clone()
                    .or_else(|| self.legacy_place_name()),
            },
            Category::Raids => ActivityDetails::Raid {
                zone_id: self.zone_id,
                zone_name: self.zone_name.clone(),
                encounter_id: self.encounter_id,
                encounter_name: self.encounter_name.clone(),
                difficulty_id: self.difficulty_id,
                difficulty: self.difficulty.clone(),
                pull: None,
                boss_percent: self.boss_percent,
            },
            Category::MythicPlus => ActivityDetails::Dungeon {
                zone_id: self.zone_id,
                dungeon_name: self.legacy_place_name(),
                map_id: self.map_id,
                keystone_level: self.keystone_level.or(self.level),
                affixes: self.affixes.clone(),
                upgrade_level: self.upgrade_level,
            },
            Category::SoloShuffle => ActivityDetails::SoloRounds {
                map_id: self.zone_id,
                map_name: self.legacy_place_name(),
                rounds_won: self.solo_shuffle_rounds_won,
                rounds_played: self.solo_shuffle_rounds_played,
                rounds: self
                    .solo_shuffle_timeline
                    .iter()
                    .map(|round| RoundSummary {
                        round: round.round,
                        outcome: if round.result {
                            Outcome::Win
                        } else {
                            Outcome::Loss
                        },
                        start_ms: seconds_to_ms(round.timestamp),
                        duration_ms: round.duration.map(seconds_to_ms),
                    })
                    .collect(),
            },
            Category::TwoVTwo
            | Category::ThreeVThree
            | Category::FiveVFive
            | Category::Skirmish
            | Category::Battlegrounds => ActivityDetails::ArenaOrBattleground {
                map_id: self.zone_id,
                map_name: self.legacy_place_name(),
                team_mmr: self.team_mmr,
            },
            Category::Manual => ActivityDetails::Manual,
        }
    }
}

/// A legacy clip stores no parent identifier, but its filename is the source
/// video name plus ` - Clipped at <date>`. Stripping that suffix rebuilds the
/// identifier the source recording has in this library.
fn legacy_clip_source_id(media_name: &str) -> RecordingId {
    let stem = media_name
        .rsplit_once(" - Clipped at ")
        .map(|(source, _)| format!("{source}.{MEDIA_EXTENSION}"))
        .unwrap_or_else(|| media_name.to_owned());
    RecordingId::from_media_name(&stem)
}

fn legacy_category(value: &str) -> Option<Category> {
    Some(match value {
        "2v2" => Category::TwoVTwo,
        "3v3" => Category::ThreeVThree,
        "5v5" => Category::FiveVFive,
        "Skirmish" => Category::Skirmish,
        "Solo Shuffle" => Category::SoloShuffle,
        "Mythic+" => Category::MythicPlus,
        "Raids" => Category::Raids,
        "Battlegrounds" => Category::Battlegrounds,
        "Clips" => Category::Clip,
        "Manual" => Category::Manual,
        _ => return None,
    })
}

fn legacy_flavor(value: Option<&str>) -> GameFlavor {
    match value {
        Some("Retail") | None => GameFlavor::Retail,
        Some("Classic") => GameFlavor::Classic,
        Some(other) => GameFlavor::Unknown(other.to_owned()),
    }
}

/// The legacy `result` boolean means different things per category; this is the
/// same mapping the activity engine writes for new recordings.
fn legacy_outcome(category: &Category, result: bool) -> Outcome {
    match category {
        Category::MythicPlus => {
            if result {
                Outcome::Complete
            } else {
                Outcome::Abandoned
            }
        }
        Category::Manual => Outcome::Unknown,
        _ => {
            if result {
                Outcome::Win
            } else {
                Outcome::Loss
            }
        }
    }
}

/// Legacy sidecars store no title; the library rebuilds a display string from
/// the fields they do store. Missing values simply drop out.
fn legacy_title(
    encounter_name: &Option<String>,
    zone_name: &Option<String>,
    player: Option<&LegacyCombatant>,
) -> String {
    let base = encounter_name
        .clone()
        .or_else(|| zone_name.clone())
        .unwrap_or_default();
    match player.and_then(|player| player.name.clone()) {
        Some(name) if !base.is_empty() => format!("{name} - {base}"),
        Some(name) => name,
        None => base,
    }
}

fn legacy_player(combatant: &LegacyCombatant) -> Option<PlayerSummary> {
    Some(PlayerSummary {
        name: combatant.name.clone()?,
        realm: combatant.realm.clone(),
        guid: combatant.guid.clone(),
        spec_id: combatant.spec_id,
    })
}

fn legacy_combatant(combatant: &LegacyCombatant) -> CombatantSummary {
    CombatantSummary {
        name: combatant.name.clone(),
        realm: combatant.realm.clone(),
        guid: combatant.guid.clone(),
        region: combatant.region.clone(),
        spec_id: combatant.spec_id,
        team_id: combatant.team_id.and_then(|team| u8::try_from(team).ok()),
    }
}

fn seconds_to_ms(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    (seconds * 1000.0).round() as u64
}

fn file_modified_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
}

/// Legacy sidecars are `JSON.stringify(metadata, null, 2)` with no trailing
/// newline; patched legacy sidecars keep that shape.
fn pretty_json(value: &Value) -> io::Result<String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// Minimal ISO-8601 UTC parser for the legacy `logStart`/`logEnd` strings
/// (`YYYY-MM-DDTHH:MM:SS[.mmm]Z`). Only their difference is used.
fn iso_epoch_ms(value: &str) -> Option<i64> {
    let number = |from: usize, to: usize| value.get(from..to)?.parse::<i64>().ok();
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let millis = value
        .get(20..23)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let days = days_from_civil(year as i32, month as i32, day as i32);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

// --- Correlation ---

/// The library's one ordering: newest first, ties broken by media path.
fn entry_order(left: &LibraryEntry, right: &LibraryEntry) -> std::cmp::Ordering {
    right
        .start_unix_ms
        .cmp(&left.start_unix_ms)
        .then_with(|| left.media_path.cmp(&right.media_path))
}

/// Correlation: identical unique hash and activity start times within one
/// minute. Clips, solo shuffle, and manual recordings only ever group with the
/// literally identical video, which for a local-only library means never.
fn correlate(entries: &[LibraryEntry]) -> Vec<CorrelatedActivity> {
    let mut correlated: Vec<CorrelatedActivity> = Vec::new();
    let mut primary_starts: Vec<i64> = Vec::new();
    let mut primaries_by_hash: HashMap<&str, Vec<usize>> = HashMap::new();

    for entry in entries {
        let start = entry.start_unix_ms;
        let matched = entry
            .activity_hash
            .as_deref()
            .filter(|_| !excluded_from_correlation(&entry.category))
            .and_then(|hash| {
                primaries_by_hash.get(hash).and_then(|positions| {
                    positions.iter().copied().find(|position| {
                        (primary_starts[*position] - start).abs() <= CORRELATION_TOLERANCE_MS
                    })
                })
            });

        if let Some(position) = matched {
            correlated[position].local_pov_ids.push(entry.id.clone());
            continue;
        }

        let position = correlated.len();
        correlated.push(CorrelatedActivity {
            primary_id: entry.id.clone(),
            local_pov_ids: Vec::new(),
        });
        primary_starts.push(start);
        if !excluded_from_correlation(&entry.category)
            && let Some(hash) = entry.activity_hash.as_deref()
        {
            primaries_by_hash.entry(hash).or_default().push(position);
        }
    }
    correlated
}

fn excluded_from_correlation(category: &Category) -> bool {
    matches!(
        category,
        Category::Clip | Category::SoloShuffle | Category::Manual
    )
}

/// Convert timeline offsets relative to the activity start into media offsets.
/// Markers that fall before the media starts are clipped away.
fn shift_timeline(
    timeline: &[TimelineItem],
    activity_start_ms: i64,
    media_start_ms: i64,
    duration_ms: u64,
) -> Vec<TimelineItem> {
    let lead_in_ms = activity_start_ms - media_start_ms;
    let mut shifted = Vec::new();
    for item in timeline {
        let start = item.start_ms() as i64 + lead_in_ms;
        let end = item.end_ms().map(|end| end as i64 + lead_in_ms);
        if start > duration_ms as i64 {
            continue;
        }
        match item.shape() {
            TimelineShape::Point => {
                if start < 0 {
                    continue;
                }
                shifted.push(TimelineItem::point(
                    item.kind().clone(),
                    start as u64,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ));
            }
            TimelineShape::Span => {
                let end = end.unwrap_or(start);
                if end < 0 {
                    continue;
                }
                let clamped_end = (end as u64).min(duration_ms);
                if let Ok(span) = TimelineItem::span(
                    item.kind().clone(),
                    (start.max(0) as u64).min(clamped_end),
                    clamped_end,
                    item.label().map(str::to_owned),
                    item.outcome(),
                    item.player_reference().map(str::to_owned),
                ) {
                    shifted.push(span);
                }
            }
        }
    }
    shifted
}

/// Shift meter fight offsets relative to the activity start into media
/// offsets, using the same signed lead-in as `shift_timeline` so meter fights
/// line up with the timeline bands. Fights wholly outside the media are
/// dropped; overlapping bounds clamp into the media and the end never precedes
/// the start. `active_ms` is activity-invariant.
fn shift_meter(
    meter: &MeterData,
    activity_start_ms: i64,
    media_start_ms: i64,
    duration_ms: u64,
) -> MeterData {
    let lead_in_ms = activity_start_ms - media_start_ms;
    MeterData {
        fights: meter
            .fights
            .iter()
            .filter_map(|fight| {
                let start = fight.start_ms as i64 + lead_in_ms;
                let end = fight.end_ms as i64 + lead_in_ms;
                if start > duration_ms as i64 || end < 0 {
                    return None;
                }
                let mut shifted = fight.clone();
                shifted.start_ms = (start.max(0) as u64).min(duration_ms);
                shifted.end_ms = (end.max(0) as u64).min(duration_ms).max(shifted.start_ms);
                shifted.first_event_ms = fight.first_event_ms.map(|first| {
                    (first as i64 + lead_in_ms)
                        .clamp(shifted.start_ms as i64, shifted.end_ms as i64)
                        as u64
                });
                let (start_ms, end_ms) = (shifted.start_ms, shifted.end_ms);
                for actor in &mut shifted.actors {
                    for spell in &mut actor.spells {
                        shift_samples(spell, lead_in_ms, start_ms, end_ms);
                        for target in &mut spell.targets {
                            shift_samples(target, lead_in_ms, start_ms, end_ms);
                        }
                    }
                    for target in &mut actor.targets {
                        shift_samples(target, lead_in_ms, start_ms, end_ms);
                    }
                }
                shifted.deaths.retain_mut(|death| {
                    let at_ms = death.at_ms as i64 + lead_in_ms;
                    if at_ms < 0 {
                        return false;
                    }
                    death.at_ms = (at_ms as u64).clamp(shifted.start_ms, shifted.end_ms);
                    death.events.retain_mut(|event| {
                        let at_ms = event.at_ms as i64 + lead_in_ms;
                        if at_ms < 0 {
                            return false;
                        }
                        event.at_ms = (at_ms as u64).min(death.at_ms);
                        true
                    });
                    true
                });
                Some(shifted)
            })
            .collect(),
    }
}

/// Move one meter entry's samples onto the media sample grid inside its
/// fight, drop those before the media start, and re-total the entry from what
/// remains.
fn shift_samples(entry: &mut MeterEntry, lead_in_ms: i64, start_ms: u64, end_ms: u64) {
    entry.samples.retain_mut(|sample| {
        let at_ms = sample.at_ms as i64 + lead_in_ms;
        if at_ms < 0 {
            return false;
        }
        sample.at_ms = (at_ms as u64)
            .div_ceil(SAMPLE_INTERVAL_MS)
            .saturating_mul(SAMPLE_INTERVAL_MS)
            .clamp(start_ms, end_ms);
        true
    });
    entry.amount = entry.samples.iter().map(|sample| sample.amount).sum();
    entry.hits = entry.samples.iter().map(|sample| sample.hits).sum();
    entry.overheal = entry.samples.iter().map(|sample| sample.overheal).sum();
}

/// Filename sanitizer: invalid characters become spaces, runs of spaces
/// collapse.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = false;
    for character in name.chars() {
        let character = match character {
            '<' | '>' | ':' | '"' | '/' | '|' | '?' | '*' | '\\' => ' ',
            other if other.is_control() => ' ',
            other => other,
        };
        if character == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(character);
            last_space = false;
        }
    }
    out.trim().to_owned()
}

/// `activity-<start>-<identifier> - <sanitized title>`: uniqueness comes from
/// the identifier, never from the title.
pub fn unique_stem(id: &RecordingId, start_unix_ms: i64, title: &str) -> String {
    let short: String = id
        .as_str()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let stem = format!("activity-{start_unix_ms}-{short}");
    let title = sanitize_name(title);
    if title.is_empty() {
        stem
    } else {
        format!("{stem} - {title}")
    }
}

fn default_title(category: &Category) -> String {
    match category {
        Category::Manual => "Manual recording".to_owned(),
        Category::Clip => "Clip".to_owned(),
        other => format!("{other:?}"),
    }
}

/// Era log sources record `Classic` in their metadata.
fn recorded_flavor(flavor: &GameFlavor) -> GameFlavor {
    match flavor {
        GameFlavor::Era => GameFlavor::Classic,
        other => other.clone(),
    }
}

fn media_has_content(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        // Scanning must never decode media, so a nonzero length is the only
        // content check here; the media worker validates real outputs.
        Ok(metadata) if metadata.is_file() => Ok(metadata.len() > 0),
        Ok(_) => Err(format!("media path {} is not a file", path.display())),
        Err(error) => Err(format!("media file {}: {error}", path.display())),
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn reason_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".recovery.txt");
    path.with_file_name(name)
}

fn write_atomic(temp: &Path, final_path: Option<&Path>, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    match final_path {
        Some(final_path) => fs::rename(temp, final_path),
        None => Ok(()),
    }
}

/// Rename, falling back to copy plus remove across filesystems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            fs::remove_file(from)
        }
    }
}

/// Wall-clock milliseconds; used for generated clip dates.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Windows cannot delete a file that is still open, and the player closes a
/// recording it was showing only shortly after being told to.
fn remove_media(path: &Path) -> io::Result<()> {
    const SHARING_VIOLATION: i32 = 32;
    for _ in 0..20 {
        match fs::remove_file(path) {
            Err(error) if cfg!(windows) && error.raw_os_error() == Some(SHARING_VIOLATION) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => return result,
        }
    }
    fs::remove_file(path)
}

/// Unique temporary root for one test tree, `wr-<prefix>-<uuid>`.
#[cfg(test)]
pub(crate) fn test_root(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wr-{prefix}-{}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    use crate::domain::{
        Codec, MeterActor, MeterDeath, MeterDeathEvent, MeterDeathEventKind, MeterEntry,
        MeterFight, MeterMetric, MeterSample, TimelineKind,
    };

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/native/fixtures/legacy/sidecars")
    }

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> Self {
            let root = test_root(&format!("storage-{name}"));
            let tree = Self { root };
            tree.storage().prepare().expect("prepare");
            fs::create_dir_all(tree.capture_root().join("replay")).expect("replay dir");
            fs::create_dir_all(tree.capture_root().join("regular")).expect("regular dir");
            tree
        }

        fn library(&self) -> PathBuf {
            self.root.join("recordings with space")
        }

        fn capture_root(&self) -> PathBuf {
            self.root.join("capture")
        }

        fn storage(&self) -> Storage {
            Storage::new(self.library(), self.capture_root())
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.library().join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Write `count` valid native sidecars, the first `protected_count` of
    /// them protected, plus a placeholder media file for each: the same
    /// on-disk shape finalize produces, without running the capture pipeline.
    fn install_native_fixtures(
        tree: &TempTree,
        count: usize,
        protected_count: usize,
    ) -> Vec<String> {
        (0..count)
            .map(|index| {
                let name = format!("native-{index:02}");
                let id = uuid::Uuid::new_v4();
                let protected = index < protected_count;
                let start = 1_772_323_200_000 + index as i64;
                let sidecar = format!(
                    r#"{{"schema_version":1,"media_file":"{name}.mp4","id":"{id}","category":"raids","flavor":"retail","title":"Native {index}","start_unix_ms":{start},"duration_ms":60000,"outcome":"unknown","protected":{protected},"combatants":[],"timeline":[],"details":{{"kind":"raid"}},"media":{{"has_content":true}}}}"#
                );
                tree.write(&format!("{name}.json"), &sidecar);
                tree.write(&format!("{name}.mp4"), "fake media bytes");
                name
            })
            .collect()
    }

    /// Copy every legacy fixture plus a placeholder media file for each.
    fn install_legacy_fixtures(tree: &TempTree) -> Vec<String> {
        let mut names = Vec::new();
        let mut entries: Vec<PathBuf> = fs::read_dir(fixture_dir())
            .expect("fixtures")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_stem()
                .expect("stem")
                .to_string_lossy()
                .into_owned();
            let json = fs::read_to_string(&path).expect("read fixture");
            tree.write(&format!("{name}.json"), &json);
            tree.write(&format!("{name}.mp4"), "fake media bytes");
            names.push(name);
        }
        names
    }

    fn draft(id: &RecordingId) -> RecordingDraft {
        RecordingDraft {
            id: id.clone(),
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
            player: Some(PlayerSummary {
                name: "Testone".to_owned(),
                realm: Some("Testrealm".to_owned()),
                guid: Some("Player-1000-AAAA0001".to_owned()),
                spec_id: Some(577),
            }),
            combatants: Vec::new(),
            timeline: vec![
                TimelineItem::point(
                    TimelineKind::Death,
                    1_000,
                    Some("Testtwo".to_owned()),
                    Some(Outcome::Loss),
                    None,
                ),
                TimelineItem::span(TimelineKind::Encounter, 0, 60_000, None, None, None)
                    .expect("span"),
            ],
            outcome: Some(Outcome::Win),
            ended_at_ms: Some(1_772_323_260_000),
            duration_ms: Some(75_000),
            title: Some("Testone - Undermine, Chrome King Gallywix [M] (Kill)".to_owned()),
            activity_hash: Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned()),
            meter: MeterData::default(),
        }
    }

    fn artifacts(tree: &TempTree, with_replay: bool) -> CaptureArtifacts {
        let regular = tree
            .capture_root()
            .join("regular/Video_2026-03-01_00-00-05.mkv");
        fs::write(&regular, "regular bytes").expect("regular");
        let replay = with_replay.then(|| {
            let replay = tree
                .capture_root()
                .join("replay/Replay_2026-03-01_00-00-00.mkv");
            fs::write(&replay, "replay bytes").expect("replay");
            replay
        });
        CaptureArtifacts {
            replay,
            regular,
            // The regular recording starts five seconds after the activity.
            regular_started_at_ms: 1_772_323_205_000,
            regular_stopped_at_ms: 1_772_323_275_000,
        }
    }

    fn meter_fixture() -> MeterData {
        MeterData {
            fights: vec![
                MeterFight {
                    label: "Chrome King Gallywix".to_owned(),
                    start_ms: 500,
                    end_ms: 58_000,
                    first_event_ms: Some(1_000),
                    active_ms: 55_000,
                    ambient: false,
                    actors: vec![MeterActor {
                        guid: "Player-1000-AAAA0001".to_owned(),
                        name: "Testone".to_owned(),
                        spells: vec![MeterEntry {
                            metric: MeterMetric::Damage,
                            key: "Smite".to_owned(),
                            marker: 0,
                            amount: 1_234,
                            hits: 10,
                            overheal: 0,
                            min: 100,
                            max: 200,
                            targets: vec![MeterEntry {
                                metric: MeterMetric::Damage,
                                key: "Chrome King Gallywix".to_owned(),
                                marker: 0,
                                amount: 1_234,
                                hits: 10,
                                overheal: 0,
                                min: 100,
                                max: 200,
                                targets: Vec::new(),
                                samples: vec![MeterSample {
                                    at_ms: 30_000,
                                    amount: 1_234,
                                    hits: 10,
                                    overheal: 0,
                                    min: 100,
                                    max: 200,
                                }],
                            }],
                            samples: vec![MeterSample {
                                at_ms: 58_500,
                                amount: 1_234,
                                hits: 10,
                                overheal: 0,
                                min: 100,
                                max: 200,
                            }],
                        }],
                        targets: Vec::new(),
                    }],
                    deaths: vec![MeterDeath {
                        guid: "Player-1000-AAAA0001".to_owned(),
                        name: "Testone".to_owned(),
                        at_ms: 2_000,
                        max_hp: 500_000,
                        events: vec![
                            MeterDeathEvent {
                                kind: MeterDeathEventKind::Healing,
                                at_ms: 0,
                                source_name: "Healer".to_owned(),
                                spell_name: "Heal".to_owned(),
                                amount: 50,
                                hp: 400_000,
                                overkill: 0,
                            },
                            MeterDeathEvent {
                                kind: MeterDeathEventKind::Damage,
                                at_ms: 1_500,
                                source_name: "Boss".to_owned(),
                                spell_name: "Hit".to_owned(),
                                amount: 100,
                                hp: 0,
                                overkill: 25,
                            },
                        ],
                    }],
                },
                // Ends beyond the media duration: the shift must clamp.
                MeterFight {
                    label: "Trash".to_owned(),
                    start_ms: 75_000,
                    end_ms: 90_000,
                    first_event_ms: None,
                    active_ms: 12_000,
                    ambient: false,
                    actors: Vec::new(),
                    deaths: Vec::new(),
                },
                // Ends before a media that starts after the activity: dropped
                // under a negative lead-in, kept and shifted otherwise.
                MeterFight {
                    label: "Pre-media".to_owned(),
                    start_ms: 0,
                    end_ms: 4_000,
                    first_event_ms: None,
                    active_ms: 3_000,
                    ambient: false,
                    actors: Vec::new(),
                    deaths: Vec::new(),
                },
                // Wholly beyond the media duration: always dropped.
                MeterFight {
                    label: "Post-media".to_owned(),
                    start_ms: 80_000,
                    end_ms: 85_000,
                    first_event_ms: None,
                    active_ms: 4_000,
                    ambient: false,
                    actors: Vec::new(),
                    deaths: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn scan_accepts_zero_byte_performance_placeholders_without_decoding() {
        let tree = TempTree::new("zero-byte-corpus");
        let storage = tree.storage();
        let names = install_native_fixtures(&tree, 3, 0);
        let media = tree.library().join(&names[0]).with_extension("mp4");
        fs::write(&media, []).expect("truncate placeholder media");

        let index = storage.scan();
        assert_eq!(index.entries.len(), names.len());
        assert!(index.skipped.is_empty());
        assert!(
            index
                .entries
                .iter()
                .find(|entry| entry.media_path == media)
                .is_some_and(|entry| !entry.media.has_content)
        );
    }

    #[test]
    fn finalize_writes_media_relative_markers_and_survives_a_rescan() {
        let tree = TempTree::new("finalize");
        let storage = tree.storage();
        let temp_media = tree.capture_root().join("staging/combined.mp4");
        fs::write(&temp_media, "final media bytes").expect("temp media");

        let id = RecordingId::new();
        let mut draft = draft(&id);
        draft.meter = meter_fixture();
        let artifacts = artifacts(&tree, true);
        let entry = storage
            .finalize(
                &draft,
                &artifacts,
                &CombinedMedia {
                    temp_media: temp_media.clone(),
                    // Eight seconds of usable replay in front of a regular
                    // recording that started five seconds after the activity.
                    actual_replay_ms: 8_000,
                    facts: MediaFacts {
                        fps: Some(60),
                        width: None,
                        height: None,
                        codec: Some(Codec::H264),
                        has_content: true,
                    },
                },
            )
            .expect("finalize");

        // Media start is five seconds before the activity, so every marker
        // shifts by the usable replay lead-in.
        assert_eq!(entry.duration_ms, 8_000 + 70_000);
        assert_eq!(entry.timeline[0].start_ms(), 4_000);
        assert_eq!(entry.timeline[0].end_ms(), None);
        assert_eq!(entry.timeline[1].start_ms(), 3_000);
        assert_eq!(entry.timeline[1].end_ms(), Some(63_000));
        // Meter fights, read back from the written sidecar, shift by the same
        // three-second lead-in, clamp to the media duration, and keep their
        // contents; the fight beyond the media end is dropped.
        let meter = load_meter(&entry.sidecar_path).expect("meter");
        assert_eq!(meter.fights.len(), 3);
        assert_eq!(meter.fights[0].start_ms, 3_500);
        assert_eq!(meter.fights[0].end_ms, 61_000);
        assert_eq!(meter.fights[0].active_ms, 55_000);
        assert_eq!(meter.fights[0].actors[0].spells[0].amount, 1_234);
        assert_eq!(
            meter.fights[0].actors[0].spells[0].samples[0].at_ms,
            meter.fights[0].end_ms
        );
        // A spell's own per-target split shifts with it.
        let nested = &meter.fights[0].actors[0].spells[0].targets[0];
        assert_eq!(nested.samples[0].at_ms, 33_000);
        assert_eq!(nested.amount, 1_234);
        assert_eq!(meter.fights[0].deaths[0].at_ms, 5_000);
        assert_eq!(
            meter.fights[0].deaths[0]
                .events
                .iter()
                .map(|event| event.at_ms)
                .collect::<Vec<_>>(),
            vec![3_000, 4_500]
        );
        assert_eq!(meter.fights[1].start_ms, 78_000);
        assert_eq!(meter.fights[1].end_ms, 78_000);
        assert_eq!(meter.fights[1].active_ms, 12_000);
        assert_eq!(meter.fights[2].start_ms, 3_000);
        assert_eq!(meter.fights[2].end_ms, 7_000);
        assert_eq!(meter.fights[2].active_ms, 3_000);
        assert!(entry.media_path.starts_with(tree.library()));
        assert!(
            entry
                .media_path
                .to_string_lossy()
                .contains("Chrome King Gallywix [M] (Kill)")
        );
        assert_eq!(
            fs::read_to_string(&entry.media_path).expect("media"),
            "final media bytes"
        );
        assert!(!temp_media.exists());
        // Intermediates are gone only after both final names exist.
        assert!(!artifacts.regular.exists());
        assert!(!artifacts.replay.expect("replay").exists());

        let index = storage.scan();
        assert_eq!(index.entries.as_ref(), &vec![entry.clone()]);
        assert_eq!(index.correlations.len(), 1);
    }

    #[test]
    fn regular_only_finalization_clips_markers_before_the_media_start() {
        let tree = TempTree::new("finalize-regular");
        let storage = tree.storage();
        let temp_media = tree.capture_root().join("staging/combined.mp4");
        fs::write(&temp_media, "final media bytes").expect("temp media");

        let entry = storage
            .finalize(
                &draft(&RecordingId::new()),
                &artifacts(&tree, false),
                &CombinedMedia {
                    temp_media,
                    actual_replay_ms: 0,
                    facts: MediaFacts {
                        fps: None,
                        width: None,
                        height: None,
                        codec: None,
                        has_content: true,
                    },
                },
            )
            .expect("finalize");

        assert_eq!(entry.duration_ms, 70_000);
        // The death at +1 s and the first five seconds of the encounter span
        // are not in the media; the point is dropped, the span is truncated.
        assert_eq!(entry.timeline.len(), 1);
        assert_eq!(entry.timeline[0].start_ms(), 0);
        assert_eq!(entry.timeline[0].end_ms(), Some(55_000));
    }

    #[test]
    fn startup_sweep_quarantines_interruption_leftovers_only() {
        let tree = TempTree::new("sweep");
        let storage = tree.storage();
        install_native_fixtures(&tree, 11, 2);
        let kept = tree.library().join("native-00.mp4");

        // One row per interruption state the architecture can actually create.
        let orphan_media = tree.write("activity-1772323200000-orphan.mp4", "media with no sidecar");
        let orphan_temp = tree.write("activity-1772323200000-orphan.json.tmp", "{}");
        let replay = tree
            .capture_root()
            .join("replay/Replay_2026-03-01_00-00-00.mkv");
        fs::write(&replay, "replay").expect("replay");
        let regular = tree
            .capture_root()
            .join("regular/Video_2026-03-01_00-00-05.mkv");
        fs::write(&regular, "regular").expect("regular");
        let staging = tree.capture_root().join("staging/replay-trim-1.mkv");
        fs::write(&staging, "trim").expect("staging");

        let report = storage.sweep_orphans(&storage.scan());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.quarantined.len(), 5);
        for original in [&orphan_media, &orphan_temp, &replay, &regular, &staging] {
            assert!(!original.exists(), "{} was not swept", original.display());
        }
        for moved in &report.quarantined {
            assert!(moved.starts_with(tree.library().join(RECOVERY_DIR)));
            let reason = moved.with_file_name(format!(
                "{}.recovery.txt",
                moved.file_name().expect("name").to_string_lossy()
            ));
            assert!(
                fs::read_to_string(reason)
                    .expect("reason")
                    .contains("original path:")
            );
        }

        // Sidecar-referenced media is untouched, and the library still scans.
        assert!(kept.exists());
        assert_eq!(storage.scan().entries.len(), 11);
    }

    #[test]
    fn updates_rewrite_native_sidecars_from_the_typed_model() {
        let tree = TempTree::new("update");
        let storage = tree.storage();

        // A native sidecar round-trips through the typed model; the meter,
        // which the entry does not hold, must survive the tag/protect rewrite.
        let temp_media = tree.capture_root().join("staging/native.mp4");
        fs::write(&temp_media, "media").expect("media");
        let mut native_draft = draft(&RecordingId::new());
        native_draft.meter = meter_fixture();
        let native = storage
            .finalize(
                &native_draft,
                &artifacts(&tree, false),
                &CombinedMedia {
                    temp_media,
                    actual_replay_ms: 0,
                    facts: MediaFacts {
                        fps: Some(60),
                        width: None,
                        height: None,
                        codec: Some(Codec::H264),
                        has_content: true,
                    },
                },
            )
            .expect("finalize");
        let written = load_meter(&native.sidecar_path).expect("finalized meter");
        let updated = storage
            .update(&native, &EntryUpdate::Protected(true))
            .expect("protect native");
        let tagged = storage
            .update(&updated, &EntryUpdate::Tag("  keeper  ".to_owned()))
            .expect("tag native");
        assert_eq!(tagged.tag.as_deref(), Some("  keeper  "));
        // Both rewrites patch the document on disk, so the meter comes back
        // unchanged.
        let meter = load_meter(&tagged.sidecar_path).expect("updated meter");
        assert_eq!(meter, written);
        // No replay here: the media starts five seconds after the activity, so
        // the lead-in is negative. The encounter fight clamps to the media
        // start, the trash fight end clamps to the 70 s media duration, and
        // the pre-media fight is dropped.
        assert_eq!(meter.fights.len(), 2);
        assert_eq!(meter.fights[0].start_ms, 0);
        assert_eq!(meter.fights[0].end_ms, 53_000);
        assert_eq!(meter.fights[0].active_ms, 55_000);
        assert_eq!(meter.fights[1].start_ms, 70_000);
        assert_eq!(meter.fights[1].end_ms, 70_000);
        let reloaded = storage
            .scan()
            .entries
            .iter()
            .find(|entry| entry.id == native.id)
            .cloned()
            .expect("rescan");
        assert_eq!(reloaded, tagged);
    }

    #[test]
    fn legacy_sidecars_load_with_their_contract_intact() {
        let tree = TempTree::new("legacy-scan");
        let names = install_legacy_fixtures(&tree);
        // One unrelated file that must be ignored.
        tree.write("notes.txt", "not a recording");

        let storage = tree.storage();
        let index = storage.scan();
        assert_eq!(index.entries.len(), names.len());
        assert!(index.skipped.is_empty(), "{:?}", index.skipped);

        let raid = index
            .entries
            .iter()
            .find(|entry| entry.category == Category::Raids)
            .expect("raid entry");
        assert_eq!(raid.outcome, Outcome::Win);
        assert_eq!(raid.start_unix_ms, 1_772_323_200_000);
        assert_eq!(raid.duration_ms, 214_500);
        assert_eq!(raid.title, "Testone - Chrome King Gallywix");
        assert!(raid.protected);
        assert_eq!(raid.tag.as_deref(), Some("good pull"));
        assert_eq!(
            raid.activity_hash.as_deref(),
            Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0")
        );
        assert_eq!(raid.media.codec, None);
        let player = raid.player.as_ref().expect("raid player");
        assert_eq!(player.name, "Testone");
        assert_eq!(player.realm.as_deref(), Some("Testrealm"));
        assert_eq!(raid.combatants.len(), 2);
        assert_eq!(
            raid.details,
            ActivityDetails::Raid {
                zone_id: Some(2769),
                zone_name: Some("Undermine".to_owned()),
                encounter_id: Some(3009),
                encounter_name: Some("Chrome King Gallywix".to_owned()),
                difficulty_id: Some(16),
                difficulty: Some("M".to_owned()),
                pull: None,
                boss_percent: Some(0),
            }
        );
        // Deaths become points at their second offsets; a friendly death is a
        // loss, a hostile one a win.
        let deaths: Vec<_> = raid
            .timeline
            .iter()
            .filter(|item| item.kind() == &TimelineKind::Death)
            .collect();
        assert_eq!(deaths.len(), 2);
        assert_eq!(deaths[0].start_ms(), 130_500);
        assert_eq!(deaths[0].outcome(), Some(Outcome::Loss));
        assert_eq!(deaths[0].label(), Some("Testtwo"));
        assert_eq!(deaths[1].start_ms(), 214_000);
        assert_eq!(deaths[1].outcome(), Some(Outcome::Win));

        let shuffle = index
            .entries
            .iter()
            .find(|entry| entry.category == Category::SoloShuffle)
            .expect("solo shuffle entry");
        assert_eq!(shuffle.outcome, Outcome::Win);
        let ActivityDetails::SoloRounds {
            rounds_won,
            rounds_played,
            rounds,
            ..
        } = &shuffle.details
        else {
            panic!("solo shuffle details");
        };
        assert_eq!(*rounds_won, Some(4));
        assert_eq!(*rounds_played, Some(6));
        assert_eq!(rounds.len(), 6);
        assert_eq!(rounds[0].outcome, Outcome::Win);
        assert_eq!(rounds[1].outcome, Outcome::Loss);
        // Round timeline items carry the same per-round outcomes.
        assert_eq!(shuffle.timeline.len(), 6);
        assert_eq!(shuffle.timeline[0].label(), Some("Round 1"));
        assert_eq!(shuffle.timeline[1].outcome(), Some(Outcome::Loss));

        let mythic = index
            .entries
            .iter()
            .find(|entry| entry.category == Category::MythicPlus)
            .expect("mythic plus entry");
        assert_eq!(mythic.outcome, Outcome::Abandoned);
        assert!(!mythic.protected);
        let ActivityDetails::Dungeon {
            keystone_level,
            affixes,
            upgrade_level,
            ..
        } = &mythic.details
        else {
            panic!("dungeon details");
        };
        assert_eq!(*keystone_level, Some(12));
        assert_eq!(affixes, &vec![9, 10, 152]);
        assert_eq!(*upgrade_level, Some(0));
        // Challenge-mode segments become spans from their ISO log stamps, the
        // bloodlust cast sits at its offset from the activity start, and the
        // friendly death is a loss.
        let span_at = |start_ms: u64| {
            mythic
                .timeline
                .iter()
                .find(|item| item.start_ms() == start_ms)
                .unwrap_or_else(|| panic!("no timeline item at {start_ms}"))
        };
        let trash = span_at(0);
        assert_eq!(trash.end_ms(), Some(160_000));
        let bloodlust = span_at(120_000);
        assert_eq!(bloodlust.kind(), &TimelineKind::Bloodlust);
        assert_eq!(bloodlust.end_ms(), Some(160_000));
        let boss = span_at(160_000);
        assert_eq!(boss.kind(), &TimelineKind::Encounter);
        assert_eq!(boss.end_ms(), Some(310_000));
        let death = span_at(230_000);
        assert_eq!(death.kind(), &TimelineKind::Death);
        assert_eq!(death.outcome(), Some(Outcome::Loss));

        let clip = index
            .entries
            .iter()
            .find(|entry| entry.category == Category::Clip)
            .expect("clip entry");
        assert_eq!(clip.start_unix_ms, 1_772_410_000_000);
        assert_eq!(clip.duration_ms, 28_000);
        assert_eq!(clip.outcome, Outcome::Win);
        let ActivityDetails::Clip {
            source_recording,
            source_category,
            source_title,
        } = &clip.details
        else {
            panic!("clip details");
        };
        // The clip resolves back to the source recording's id via its filename.
        assert_eq!(source_recording.as_str(), raid.id.as_str());
        assert_eq!(source_category, &Category::Raids);
        assert_eq!(source_title.as_deref(), Some("Chrome King Gallywix"));

        // Reading the library never rewrites a legacy sidecar.
        for name in names {
            let original =
                fs::read_to_string(fixture_dir().join(format!("{name}.json"))).expect("fixture");
            let on_disk =
                fs::read_to_string(tree.library().join(format!("{name}.json"))).expect("scanned");
            assert_eq!(original, on_disk, "{name} was modified");
        }
    }

    #[test]
    fn updates_patch_legacy_sidecars_in_place() {
        let tree = TempTree::new("legacy-update");
        let storage = tree.storage();
        tree.write(
            "arena-2v2.json",
            r#"{"category":"2v2","duration":60,"start":1,"result":true,"teamMMR":1850,"uniqueHash":"aa00bb11cc22dd33ee44ff5566778899"}"#,
        );
        tree.write("arena-2v2.mp4", "media");
        let legacy = storage
            .scan()
            .entries
            .first()
            .cloned()
            .expect("legacy entry");

        let tagged = storage
            .update(&legacy, &EntryUpdate::Tag(" nice one ".to_owned()))
            .expect("tag");
        let protected = storage
            .update(&tagged, &EntryUpdate::Protected(true))
            .expect("protect");
        assert_eq!(protected.tag.as_deref(), Some(" nice one "));
        assert!(protected.protected);

        let patched: Value =
            serde_json::from_str(&fs::read_to_string(&legacy.sidecar_path).expect("read"))
                .expect("json");
        assert_eq!(patched["tag"], Value::String(" nice one ".to_owned()));
        assert_eq!(patched["protected"], Value::Bool(true));
        // Unknown and legacy-only fields survive untouched, and no schema
        // version is added: the sidecar keeps its legacy schema.
        assert_eq!(patched["teamMMR"], Value::from(1850));
        assert_eq!(
            patched["uniqueHash"],
            Value::from("aa00bb11cc22dd33ee44ff5566778899")
        );
        assert!(patched.get("schema_version").is_none());

        // Clearing removes the key exactly like the baseline's undefined tag.
        storage
            .update(&protected, &EntryUpdate::Tag("   ".to_owned()))
            .expect("clear tag");
        let cleared: Value =
            serde_json::from_str(&fs::read_to_string(&legacy.sidecar_path).expect("read"))
                .expect("json");
        assert!(cleared.get("tag").is_none());
        assert_eq!(cleared["protected"], Value::Bool(true));
    }

    #[test]
    fn legacy_categories_flavors_and_clips_map_or_reject() {
        let tree = TempTree::new("legacy-arms");
        let storage = tree.storage();
        tree.write(
            "arena-classic.json",
            r#"{"category":"2v2","duration":60,"start":1,"result":true,"teamMMR":1850,"flavour":"Classic"}"#,
        );
        tree.write("arena-classic.mp4", "media");
        tree.write(
            "battleground.json",
            r#"{"category":"Battlegrounds","duration":60,"start":2,"result":false,"zoneID":30}"#,
        );
        tree.write("battleground.mp4", "media");
        tree.write(
            "manual.json",
            r#"{"category":"Manual","duration":10,"start":3}"#,
        );
        tree.write("manual.mp4", "media");
        tree.write(
            "era-raid.json",
            r#"{"category":"Raids","duration":10,"start":4,"flavour":"Era"}"#,
        );
        tree.write("era-raid.mp4", "media");
        tree.write(
            "unknown-category.json",
            r#"{"category":"Brawls","duration":10,"start":5}"#,
        );
        tree.write("unknown-category.mp4", "media");
        tree.write(
            "parentless-clip.json",
            r#"{"category":"Clips","duration":10,"start":6}"#,
        );
        tree.write("parentless-clip.mp4", "media");
        tree.write(
            "unknown-parent-clip.json",
            r#"{"category":"Clips","parentCategory":"Brawls","duration":10,"start":7}"#,
        );
        tree.write("unknown-parent-clip.mp4", "media");

        let index = storage.scan();
        assert_eq!(index.entries.len(), 4);
        assert_eq!(index.skipped.len(), 3);
        assert!(index.skipped.iter().any(|skipped| {
            skipped
                .reason
                .contains("unknown legacy category \"Brawls\"")
        }));
        assert!(index.skipped.iter().any(|skipped| {
            skipped
                .reason
                .contains("legacy clip without a parent category")
        }));
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("unknown legacy parent category"))
        );

        let entry_of = |name: &str| {
            index
                .entries
                .iter()
                .find(|entry| entry.id.as_str() == format!("{name}.mp4"))
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        let arena = entry_of("arena-classic");
        assert_eq!(arena.flavor, GameFlavor::Classic);
        assert_eq!(arena.outcome, Outcome::Win);
        assert_eq!(
            arena.details,
            ActivityDetails::ArenaOrBattleground {
                map_id: None,
                map_name: None,
                team_mmr: Some(1850),
            }
        );
        let battleground = entry_of("battleground");
        assert_eq!(battleground.outcome, Outcome::Loss);
        assert!(matches!(
            battleground.details,
            ActivityDetails::ArenaOrBattleground {
                map_id: Some(30),
                ..
            }
        ));
        let manual = entry_of("manual");
        assert_eq!(manual.flavor, GameFlavor::Retail);
        assert_eq!(manual.outcome, Outcome::Unknown);
        assert_eq!(manual.details, ActivityDetails::Manual);
        // An unrecognized flavour string stays verbatim instead of guessing.
        let era = entry_of("era-raid");
        assert_eq!(era.flavor, GameFlavor::Unknown("Era".to_owned()));
    }

    #[test]
    fn deletion_reports_per_entry_failures_and_refuses_paths_outside_the_root() {
        let tree = TempTree::new("delete");
        let storage = tree.storage();
        install_native_fixtures(&tree, 3, 0);
        let index = storage.scan();

        let good = index.entries[0].clone();
        let missing = index.entries[1].clone();
        fs::remove_file(&missing.media_path).expect("remove media");
        let mut outside = index.entries[2].clone();
        outside.media_path = tree.root.join("escaped.mp4");
        fs::write(&outside.media_path, "outside").expect("outside");

        let result = storage.delete(&[good.clone(), missing.clone(), outside.clone()]);
        assert_eq!(result.deleted, vec![good.id.clone()]);
        assert_eq!(result.failures.len(), 2);
        assert!(result.failures[0].1.contains("media"));
        assert!(result.failures[1].1.contains("not a direct child"));
        assert!(!good.media_path.exists() && !good.sidecar_path.exists());
        assert!(outside.media_path.exists());

        // The sidecar of a partially removed entry stays for the next scan.
        assert!(missing.sidecar_path.exists());
    }

    #[test]
    fn storage_limits_evict_only_unprotected_recordings_oldest_first() {
        let tree = TempTree::new("evict");
        let storage = tree.storage();
        install_native_fixtures(&tree, 11, 2);
        let entries = storage.scan().entries;

        // Unlimited never evicts.
        let unlimited = storage.enforce_limit(StorageLimit::Unlimited, &entries);
        assert!(unlimited.evicted.is_empty());
        assert!(unlimited.remaining_bytes > 0);
        assert_eq!(storage.scan().entries.len(), entries.len());

        // A limit far above usage evicts nothing either.
        let gib = StorageLimit::Gib(NonZeroU64::new(1).expect("nonzero"));
        assert!(storage.enforce_limit(gib, &entries).evicted.is_empty());

        // Shrink the limit below the protected content to prove the report.
        // Sparse files give the media its size without writing it.
        let mut protected_entries: Vec<LibraryEntry> = entries
            .iter()
            .filter(|entry| entry.protected)
            .cloned()
            .collect();
        assert_eq!(protected_entries.len(), 2);
        protected_entries.push(
            entries
                .iter()
                .find(|entry| !entry.protected)
                .expect("unprotected")
                .clone(),
        );
        for entry in &protected_entries {
            File::create(&entry.media_path)
                .and_then(|file| file.set_len(512 * 1024 * 1024))
                .expect("size media");
        }
        let result = storage.enforce_limit(gib, &protected_entries);
        assert!(result.protected_over_limit);
        assert_eq!(result.evicted.len(), 1);
        let evicted = protected_entries
            .iter()
            .find(|entry| result.evicted.contains(&entry.id))
            .expect("evicted entry");
        assert!(!evicted.protected);
        assert!(!evicted.media_path.exists());
        for entry in protected_entries.iter().filter(|entry| entry.protected) {
            assert!(entry.media_path.exists(), "protected entry was evicted");
        }
    }

    #[test]
    fn invalid_sidecars_are_skipped_with_a_diagnostic() {
        let tree = TempTree::new("skip");
        let storage = tree.storage();
        tree.write("broken.json", "{ not json");
        tree.write("broken.mp4", "media");
        tree.write(
            "no-media.json",
            r#"{"schema_version":1,"media_file":"missing.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","category":"raids","flavor":"retail","title":"No media","start_unix_ms":1,"duration_ms":10,"outcome":"unknown","protected":false,"combatants":[],"timeline":[],"details":{"kind":"raid"},"media":{"has_content":true}}"#,
        );
        tree.write("no-category.json", r#"{"schema_version":1,"media_file":"no-category.mp4","id":"0d8a0e10-1a2b-4c3d-8e4f-aabbccddeeff","flavor":"retail","title":"No category","start_unix_ms":1,"duration_ms":10,"outcome":"unknown","protected":false,"combatants":[],"timeline":[],"details":{"kind":"raid"},"media":{"has_content":true}}"#);
        tree.write("no-category.mp4", "media");

        let index = storage.scan();
        assert!(index.entries.is_empty());
        assert_eq!(index.skipped.len(), 3);
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("invalid JSON"))
        );
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("media file"))
        );
        assert!(
            index
                .skipped
                .iter()
                .any(|skipped| skipped.reason.contains("missing field `category`"))
        );
    }

    /// The sweep resolves references from sidecars whatever the scanner thinks
    /// of them: a sidecar that names its media and the sibling fallback when
    /// no `media_file` is found; malformed JSON references nothing and its
    /// media is swept.
    #[test]
    fn sweep_honors_references_from_sidecars_that_fail_to_load() {
        let tree = TempTree::new("sweep-references");
        let storage = tree.storage();
        tree.write(
            "rejected.json",
            r#"{"schema_version":1,"media_file":"named.mp4"}"#,
        );
        tree.write("named.mp4", "media");
        tree.write("sibling.json", r#"{"category":"Raids","duration":10}"#);
        tree.write("sibling.mp4", "media");
        // Malformed JSON references nothing, so its media is swept.
        tree.write("truncated.json", r#"{"media_file":"lost.mp4""#);
        tree.write("lost.mp4", "media");
        let orphan = tree.write("orphan.mp4", "unreferenced media");

        let report = storage.sweep_orphans(&storage.scan());
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.quarantined.len(), 2);
        for moved in &report.quarantined {
            assert!(moved.starts_with(tree.library().join(RECOVERY_DIR)));
        }
        assert!(!orphan.exists());
        assert!(!tree.library().join("lost.mp4").exists());
        assert!(tree.library().join("named.mp4").exists());
        assert!(tree.library().join("sibling.mp4").exists());
    }
}
