// SPDX-License-Identifier: GPL-3.0-or-later

//! Incremental, coordinator-polled combat-log file reading.

use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::domain::GameFlavor;
use crate::parser::{ParseTimeContext, ParsedEvent, combat_log_version, event_name, parse_line};

const READ_CHUNK_BYTES: usize = 64 * 1024;
const CHECKPOINT_BYTES: usize = 64;
/// A new combat log only appears at session boundaries. Directory mtime makes
/// discovery immediate in the normal case; this interval is the fallback for
/// filesystems whose directory timestamps are coarse or unreliable.
const ACTIVE_FILE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum LogError {
    Io { path: PathBuf, source: io::Error },
    NoActiveLog(PathBuf),
}

impl fmt::Display for LogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NoActiveLog(path) => {
                write!(formatter, "no WoWCombatLog*.txt file in {}", path.display())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

pub struct LogTailer {
    source: PathBuf,
    path: PathBuf,
    flavor: GameFlavor,
    identity: FileIdentity,
    offset: u64,
    incomplete: Vec<u8>,
    incomplete_offset: u64,
    line_number: u64,
    base_context: ParseTimeContext,
    time_context: ParseTimeContext,
    checkpoint: Vec<u8>,
    observed_len: u64,
    observed_modified: Option<SystemTime>,
    source_modified: Option<SystemTime>,
    next_active_refresh: Instant,
    /// The last poll error logged, so a persistent one is logged only once.
    last_error: Option<String>,
}

impl LogTailer {
    /// Open a configured Logs directory (or a concrete log file) for live use.
    /// Existing bytes are deliberately ignored.
    pub fn open(
        source: PathBuf,
        flavor: GameFlavor,
        time_context: ParseTimeContext,
    ) -> Result<Self, LogError> {
        let path = active_path(&source)?;
        let metadata = metadata(&path)?;
        let identity = file_identity(&metadata);
        let offset = metadata.len();
        let checkpoint = read_checkpoint(&path, offset)?;
        let source_modified = (!source.is_file())
            .then(|| fs::metadata(&source).ok()?.modified().ok())
            .flatten();
        let seeded_context = probe_header_context(&path, time_context)?;
        Ok(Self {
            source,
            path,
            flavor,
            identity,
            offset,
            incomplete: Vec::new(),
            incomplete_offset: offset,
            line_number: 0,
            base_context: time_context,
            time_context: seeded_context,
            checkpoint,
            observed_len: metadata.len(),
            observed_modified: metadata.modified().ok(),
            source_modified,
            next_active_refresh: Instant::now() + ACTIVE_FILE_REFRESH_INTERVAL,
            last_error: None,
        })
    }

    /// Read whatever was appended since the last poll. Errors are also
    /// logged here, once per distinct message, since polling repeats them
    /// every tick until the folder recovers.
    pub fn poll(&mut self) -> Result<Vec<ParsedEvent>, LogError> {
        let result = self.read_appended();
        match &result {
            Ok(_) => self.last_error = None,
            Err(error) => {
                let message = error.to_string();
                if self.last_error.as_ref() != Some(&message) {
                    tracing::warn!(error = %message, "log poll failed");
                    self.last_error = Some(message);
                }
            }
        }
        result
    }

    fn read_appended(&mut self) -> Result<Vec<ParsedEvent>, LogError> {
        self.refresh_active_file()?;
        let current_metadata = metadata(&self.path)?;
        let current_identity = file_identity(&current_metadata);
        let current_modified = current_metadata.modified().ok();
        let metadata_changed = current_metadata.len() != self.observed_len
            || current_modified != self.observed_modified;
        let reset = current_identity != self.identity
            || current_metadata.len() < self.offset
            || (metadata_changed && !self.checkpoint_matches()?);
        if reset {
            self.reset_for_file(current_identity);
        }

        let available = current_metadata.len().saturating_sub(self.offset);
        self.observed_len = current_metadata.len();
        self.observed_modified = current_modified;
        if available == 0 {
            return Ok(Vec::new());
        }
        let mut file = File::open(&self.path).map_err(|source| self.io_error(source))?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| self.io_error(source))?;
        let mut remaining = available;
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        let mut events = Vec::new();
        while remaining > 0 {
            let amount = remaining.min(READ_CHUNK_BYTES as u64) as usize;
            let bytes_read = file
                .read(&mut buffer[..amount])
                .map_err(|source| self.io_error(source))?;
            if bytes_read == 0 {
                break;
            }
            let read_start = self.offset;
            self.offset += bytes_read as u64;
            remaining -= bytes_read as u64;
            events.extend(self.consume(&buffer[..bytes_read], read_start));
        }
        self.checkpoint = read_checkpoint(&self.path, self.offset)?;
        Ok(events)
    }

    fn refresh_active_file(&mut self) -> Result<(), LogError> {
        if self.source.is_file() {
            return Ok(());
        }
        let source_metadata = metadata(&self.source)?;
        let source_modified = source_metadata.modified().ok();
        let now = Instant::now();
        if source_modified == self.source_modified && now < self.next_active_refresh {
            return Ok(());
        }
        let active = active_path(&self.source)?;
        self.source_modified = source_modified;
        self.next_active_refresh = now + ACTIVE_FILE_REFRESH_INTERVAL;
        if active == self.path {
            return Ok(());
        }
        let metadata = metadata(&active)?;
        self.path = active;
        self.reset_for_file(file_identity(&metadata));
        Ok(())
    }

    fn reset_for_file(&mut self, identity: FileIdentity) {
        self.identity = identity;
        self.offset = 0;
        self.incomplete.clear();
        self.incomplete_offset = 0;
        self.line_number = 0;
        self.checkpoint.clear();
        self.observed_len = 0;
        self.observed_modified = None;
        // A rotated-in or truncated file starts from the caller's base
        // context; consuming its header reseeds the layout if it has one.
        self.time_context = self.base_context;
    }

    fn checkpoint_matches(&self) -> Result<bool, LogError> {
        if self.checkpoint.is_empty() {
            return Ok(true);
        }
        Ok(read_checkpoint(&self.path, self.offset)? == self.checkpoint)
    }

    fn consume(&mut self, bytes: &[u8], read_start: u64) -> Vec<ParsedEvent> {
        let mut pending = std::mem::take(&mut self.incomplete);
        if pending.is_empty() {
            self.incomplete_offset = read_start;
        }
        pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut consumed = 0;

        while let Some(relative_end) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + relative_end;
            let offset = self.incomplete_offset + consumed as u64;
            self.line_number += 1;
            let line_end = if pending.get(end.wrapping_sub(1)) == Some(&b'\r') {
                end - 1
            } else {
                end
            };
            let line = &pending[consumed..line_end];
            if !line.is_empty() {
                self.consume_line(line, offset, &mut events);
            }
            consumed = end + 1;
        }

        if consumed != 0 {
            let remaining = pending.len() - consumed;
            pending.copy_within(consumed.., 0);
            pending.truncate(remaining);
            self.incomplete_offset += consumed as u64;
        }
        self.incomplete = pending;
        events
    }

    fn consume_line(&mut self, bytes: &[u8], offset: u64, events: &mut Vec<ParsedEvent>) {
        if bytes.is_empty() {
            return;
        }
        let Ok(line) = std::str::from_utf8(bytes) else {
            tracing::debug!(
                file = %self.path.display(),
                line = self.line_number,
                offset,
                "skipped combat log line with invalid UTF-8"
            );
            return;
        };
        if let Some(version) = combat_log_version(line) {
            self.time_context = self.base_context.with_combat_log_version(version);
        }
        match parse_line(self.flavor.clone(), self.time_context, line) {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            // Only the event name is logged, never the line's contents.
            Err(failure) => tracing::debug!(
                ?failure,
                event = event_name(line).map(|name| name.chars().take(64).collect::<String>()),
                file = %self.path.display(),
                line = self.line_number,
                offset,
                "skipped unparseable combat log line"
            ),
        }
    }

    fn io_error(&self, source: io::Error) -> LogError {
        LogError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

fn active_path(source: &Path) -> Result<PathBuf, LogError> {
    if source.is_file() {
        return Ok(source.to_owned());
    }
    let entries = fs::read_dir(source).map_err(|source_error| LogError::Io {
        path: source.to_owned(),
        source: source_error,
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source_error| LogError::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("WoWCombatLog") || !name.ends_with(".txt") {
            continue;
        }
        let metadata = entry.metadata().map_err(|source_error| LogError::Io {
            path: entry.path(),
            source: source_error,
        })?;
        if metadata.is_file() {
            candidates.push((metadata.modified().ok(), name.into_owned(), entry.path()));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    candidates
        .pop()
        .map(|(_, _, path)| path)
        .ok_or_else(|| LogError::NoActiveLog(source.to_owned()))
}

fn metadata(path: &Path) -> Result<Metadata, LogError> {
    fs::metadata(path).map_err(|source| LogError::Io {
        path: path.to_owned(),
        source,
    })
}

fn read_checkpoint(path: &Path, offset: u64) -> Result<Vec<u8>, LogError> {
    let amount = offset.min(CHECKPOINT_BYTES as u64) as usize;
    if amount == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|source| LogError::Io {
        path: path.to_owned(),
        source,
    })?;
    file.seek(SeekFrom::Start(offset - amount as u64))
        .map_err(|source| LogError::Io {
            path: path.to_owned(),
            source,
        })?;
    let mut checkpoint = vec![0; amount];
    file.read_exact(&mut checkpoint)
        .map_err(|source| LogError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(checkpoint)
}

/// Seeds the active parse context from a complete, newline-terminated first
/// line without touching offset/checkpoint/incomplete state: a live open
/// starts at EOF and would otherwise never see the header. A fresh partial
/// first line is not a header yet, and an unreadable version keeps the base.
fn probe_header_context(path: &Path, base: ParseTimeContext) -> Result<ParseTimeContext, LogError> {
    let mut file = File::open(path).map_err(|source| LogError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    let mut filled = 0;
    let newline = loop {
        let read = file
            .read(&mut buffer[filled..])
            .map_err(|source| LogError::Io {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break None;
        }
        if let Some(index) = buffer[filled..filled + read]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            break Some(filled + index);
        }
        filled += read;
        if filled == buffer.len() {
            break None;
        }
    };
    let Some(end) = newline else {
        return Ok(base);
    };
    let line_end = if end > 0 && buffer[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    let Ok(line) = std::str::from_utf8(&buffer[..line_end]) else {
        return Ok(base);
    };
    Ok(combat_log_version(line).map_or(base, |version| base.with_combat_log_version(version)))
}

/// Windows has no stable inode through std; a file's creation time tells a
/// rotated log apart from the one it replaced just as well.
#[cfg(windows)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    use std::os::windows::fs::MetadataExt;
    FileIdentity {
        device: 0,
        inode: metadata.creation_time(),
    }
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::CombatEvent;
    use std::io::Write;

    const CONTEXT: ParseTimeContext = ParseTimeContext::new(2026, 0);
    const EVENT: &str =
        "4/9 19:27:13.200  ENCOUNTER_START,9999,\"Training Construct\",16,20,777,1\n";
    const V22_HEADER: &str = "8/11/2026 18:28:29.3992  COMBAT_LOG_VERSION,22,ADVANCED_LOG_ENABLED,1,BUILD_VERSION,12.1.0,PROJECT_ID,1\n";
    const V22_DAMAGE: &str = "5/24 20:26:10.911  SPELL_DAMAGE,Player-1322-07763A7B,\"Xiaohuli\",0x511,0x0,Creature-0-3013-0-11406-74284-0000266503,\"Cutpurse\",0x10a48,0x0,585,\"Smite\",0x2,Creature-0-3013-0-11406-74284-0000266503,0000000000000000,105,152,0,0,189,2084,0,0,0,250000,250000,0,0,0,0,0,0,46,0,2,0,0,0,1,0,0,0,0.000,1,1\n";
    const LEGACY_DAMAGE: &str = "5/24 20:26:10.911  SPELL_DAMAGE,Player-1322-07763A7B,\"Xiaohuli\",0x511,0x0,Creature-0-3013-0-11406-74284-0000266503,\"Cutpurse\",0x10a48,0x0,585,\"Smite\",0x2,Creature-0-3013-0-11406-74284-0000266503,0000000000000000,105,152,0,0,189,2084,0,0,0,0,0,0,0,0,0,46,0,2,0,0,0,1,0,0,0,0.000,1,1\n";

    fn test_directory() -> PathBuf {
        let path = crate::storage::test_root("logwatch");
        fs::create_dir(&path).unwrap();
        path
    }

    /// A live tailer that sees `bytes` from the start: it opens on an empty
    /// file, which then receives them.
    fn tailer_with(path: &Path, bytes: &[u8]) -> LogTailer {
        fs::write(path, b"").unwrap();
        let tailer = LogTailer::open(path.to_path_buf(), GameFlavor::Retail, CONTEXT).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
        tailer
    }

    #[test]
    fn retained_event_split_mid_line_is_emitted_once() {
        // Empty first half, inside the timestamp, at the event name, just
        // before the newline, and the whole line at once.
        for split in [0, 5, 18, EVENT.len() - 1, EVENT.len()] {
            let directory = test_directory();
            let path = directory.join("WoWCombatLog.txt");
            let mut tailer = tailer_with(&path, &EVENT.as_bytes()[..split]);
            let mut events = tailer.poll().unwrap();
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&EVENT.as_bytes()[split..]).unwrap();
            events.extend(tailer.poll().unwrap());
            assert_eq!(events.len(), 1, "split at {split}");
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn one_poll_drains_the_available_file_snapshot() {
        let directory = test_directory();
        let path = directory.join("WoWCombatLog.txt");
        let event_count = READ_CHUNK_BYTES / EVENT.len() + 2;
        let mut tailer = tailer_with(&path, EVENT.repeat(event_count).as_bytes());

        assert_eq!(tailer.poll().unwrap().len(), event_count);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn live_starts_at_eof_and_crlf_is_accepted() {
        let directory = test_directory();
        let path = directory.join("WoWCombatLog.txt");
        fs::write(&path, EVENT).unwrap();
        let mut live = LogTailer::open(path.clone(), GameFlavor::Retail, CONTEXT).unwrap();
        assert!(live.poll().unwrap().is_empty());
        let mut crlf = tailer_with(&path, EVENT.replace('\n', "\r\n").as_bytes());
        assert_eq!(crlf.poll().unwrap().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_file_truncation_and_new_active_file_restart_at_zero() {
        let directory = test_directory();
        let first = directory.join("WoWCombatLog-1.txt");
        fs::write(&first, vec![b'x'; EVENT.len() * 2]).unwrap();
        let mut tailer = LogTailer::open(directory.clone(), GameFlavor::Retail, CONTEXT).unwrap();
        fs::write(&first, EVENT).unwrap();
        assert_eq!(tailer.poll().unwrap().len(), 1);

        let second = directory.join("WoWCombatLog-2.txt");
        fs::write(&second, EVENT).unwrap();
        assert_eq!(tailer.poll().unwrap().len(), 1);
        assert_eq!(tailer.path, second);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_inode_truncate_and_regrow_past_offset_restarts_at_zero() {
        let directory = test_directory();
        let path = directory.join("WoWCombatLog.txt");
        let old_event = EVENT.replace("Training Construct", "Retired Construct");
        fs::write(&path, old_event.repeat(2)).unwrap();
        let original_identity = file_identity(&metadata(&path).unwrap());
        let mut tailer = LogTailer::open(path.clone(), GameFlavor::Retail, CONTEXT).unwrap();

        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.write_all(EVENT.repeat(3).as_bytes()).unwrap();
        file.flush().unwrap();
        assert_eq!(file_identity(&metadata(&path).unwrap()), original_identity);
        assert_eq!(tailer.poll().unwrap().len(), 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn live_at_eof_seeds_the_layout_from_a_complete_header_line() {
        let directory = test_directory();
        let path = directory.join("WoWCombatLog.txt");
        fs::write(&path, format!("{V22_HEADER}{V22_DAMAGE}")).unwrap();
        let mut tailer = LogTailer::open(path.clone(), GameFlavor::Retail, CONTEXT).unwrap();
        assert!(tailer.poll().unwrap().is_empty());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(V22_DAMAGE.as_bytes()).unwrap();
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            CombatEvent::Damage { amount: 46, .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rotation_restores_the_base_layout_until_a_new_header_reseeds() {
        let directory = test_directory();
        let first = directory.join("WoWCombatLog-1.txt");
        fs::write(&first, format!("{V22_HEADER}{V22_DAMAGE}")).unwrap();
        let mut tailer = LogTailer::open(directory.clone(), GameFlavor::Retail, CONTEXT).unwrap();
        // Live starts at EOF but the probe already seeded the v22 layout;
        // nothing is emitted from the pre-existing bytes.
        assert!(tailer.poll().unwrap().is_empty());

        // A headerless file must fall back to the base 17-field layout: the
        // amount is at index 29, not the stale v22 index 31.
        let second = directory.join("WoWCombatLog-2.txt");
        fs::write(&second, LEGACY_DAMAGE).unwrap();
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            CombatEvent::Damage { amount: 46, .. }
        ));
        assert_eq!(tailer.path, second);

        // A new file with its own header reseeds the wider layout.
        let third = directory.join("WoWCombatLog-3.txt");
        fs::write(&third, format!("{V22_HEADER}{V22_DAMAGE}")).unwrap();
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            CombatEvent::Damage { amount: 46, .. }
        ));
        assert_eq!(tailer.path, third);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn headerless_and_unreadable_headers_keep_the_legacy_layout_silently() {
        let directory = test_directory();
        let path = directory.join("WoWCombatLog.txt");
        let mut tailer = tailer_with(&path, LEGACY_DAMAGE.as_bytes());
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            CombatEvent::Damage { amount: 46, .. }
        ));

        let path = directory.join("WoWCombatLog-bad-header.txt");
        let mut tailer = tailer_with(
            &path,
            format!("8/11/2026 18:28:29.3992  COMBAT_LOG_VERSION,garbage,ADVANCED_LOG_ENABLED,1\n{LEGACY_DAMAGE}").as_bytes(),
        );
        let events = tailer.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            CombatEvent::Damage { amount: 46, .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}
