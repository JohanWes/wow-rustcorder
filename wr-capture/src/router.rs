// SPDX-License-Identifier: GPL-3.0-or-later

//! The replay buffer and the two output files, driven by GSR-style requests.
//!
//! Encoded packets stay in RAM for `keep` plus one GOP. `save-replay` writes
//! the buffer from the keyframe at or before the requested pre-roll and keeps
//! appending live packets; `start-replay-recording` asks for a keyframe, and on
//! that keyframe the replay file closes and the regular file opens. The two
//! files therefore join without a gap or an overlap.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::mkv::MkvWriter;
use crate::packet::{Packet, VideoCodec};
use crate::ts::TsWriter;

/// MPEG-TS for H.264/HEVC, Matroska for AV1.
enum Writer {
    Ts(TsWriter<BufWriter<File>>),
    Mkv(MkvWriter<BufWriter<File>>),
}

impl Writer {
    fn write(&mut self, packet: &Packet) -> io::Result<()> {
        match self {
            Self::Ts(writer) => writer.write(packet),
            Self::Mkv(writer) => writer.write(packet),
        }
    }

    fn finish(self) -> io::Result<()> {
        match self {
            Self::Ts(writer) => writer.finish(),
            Self::Mkv(writer) => writer.finish(),
        }
    }
}

pub struct Router {
    codec: VideoCodec,
    size: (u32, u32),
    audio: bool,
    keep: i64,
    replay_dir: PathBuf,
    regular_dir: PathBuf,
    ring: VecDeque<Packet>,
    /// Times of the video keyframes still in `ring`, oldest first.
    keys: VecDeque<i64>,
    /// Saved replay still taking live packets until the cut, with its
    /// request id.
    replay: Option<(Writer, PathBuf, i64)>,
    start_pending: bool,
    regular: Option<(Writer, PathBuf)>,
    /// Set when a keyframe is wanted soon; the video thread clears it.
    pub want_keyframe: bool,
}

impl Router {
    pub fn new(
        codec: VideoCodec,
        size: (u32, u32),
        audio: bool,
        keep_seconds: u32,
        replay_dir: PathBuf,
        regular_dir: PathBuf,
    ) -> Self {
        Self {
            codec,
            size,
            audio,
            keep: i64::from(keep_seconds) * 10_000_000,
            replay_dir,
            regular_dir,
            ring: VecDeque::new(),
            keys: VecDeque::new(),
            replay: None,
            start_pending: false,
            regular: None,
            want_keyframe: false,
        }
    }

    /// Handle one request line; returns the replies to send now.
    pub fn request(&mut self, request: &Value, now_ms: i64) -> Vec<Value> {
        let id = request["id"].as_i64().unwrap_or(0);
        let reply = |ok: bool, data: Option<String>| {
            let mut reply = json!({ "id": id, "result": if ok { "ok" } else { "error" } });
            if let Some(data) = data {
                reply["data"] = data.into();
            }
            reply
        };
        match request["name"].as_str().unwrap_or("") {
            "save-replay" => {
                let seconds = request["data"]["seconds"].as_i64().unwrap_or(0).max(0);
                if self.replay.is_some() {
                    return vec![reply(false, Some("a replay is already being saved".into()))];
                }
                match self.save(seconds * 10_000_000, id, now_ms) {
                    Ok(()) => {
                        self.want_keyframe = true;
                        Vec::new()
                    }
                    Err(message) => vec![reply(false, Some(message))],
                }
            }
            "start-replay-recording" => {
                if self.regular.is_none() {
                    self.start_pending = true;
                    self.want_keyframe = true;
                }
                vec![reply(true, None)]
            }
            "stop-replay-recording" => match self.regular.take() {
                Some((writer, path)) => match writer.finish() {
                    Ok(()) => vec![reply(true, Some(path.display().to_string()))],
                    Err(error) => vec![reply(
                        false,
                        Some(format!("failed to write the recording: {error}")),
                    )],
                },
                // Like GSR, a stop before the start took effect is refused;
                // the app retries it.
                None => vec![reply(false, Some("no recording is running".into()))],
            },
            _ => vec![reply(false, Some("unknown request".into()))],
        }
    }

    /// Route one encoded packet; returns replies completed by it.
    pub fn push(&mut self, packet: Packet, now_ms: i64) -> Vec<Value> {
        let mut replies = Vec::new();
        if packet.is_keyframe() && (self.replay.is_some() || self.start_pending) {
            if let Some((writer, path, id)) = self.replay.take() {
                replies.push(match writer.finish() {
                    Ok(()) => json!({ "id": id, "result": "ok", "data": path.display().to_string() }),
                    Err(error) => json!({ "id": id, "result": "error", "data": format!("failed to write the replay: {error}") }),
                });
            }
            if std::mem::take(&mut self.start_pending) {
                match self.open(&self.regular_dir, &format!("Video_{now_ms}")) {
                    Ok(opened) => self.regular = Some(opened),
                    Err(error) => eprintln!("cannot create the recording: {error}"),
                }
            }
        }
        let written = if let Some((writer, path)) = self.regular.as_mut() {
            writer.write(&packet).map_err(|error| (error, path.clone()))
        } else if let Some((writer, path, _)) = self.replay.as_mut() {
            writer.write(&packet).map_err(|error| (error, path.clone()))
        } else {
            Ok(())
        };
        if let Err((error, path)) = written {
            eprintln!("write to {} failed: {error}", path.display());
        }
        self.remember(packet);
        replies
    }

    /// Flush whatever is open; called on exit.
    pub fn finish(&mut self) {
        if let Some((writer, _, _)) = self.replay.take() {
            let _ = writer.finish();
        }
        if let Some((writer, _)) = self.regular.take() {
            let _ = writer.finish();
        }
    }

    fn save(&mut self, preroll: i64, id: i64, now_ms: i64) -> Result<(), String> {
        let newest = self.ring.back().map_or(0, |packet| packet.pts);
        let wanted = newest - preroll;
        let Some(&from) = self
            .keys
            .iter()
            .rev()
            .find(|&&key| key <= wanted)
            .or(self.keys.front())
        else {
            return Err("failed to find a video keyframe".into());
        };
        let (mut writer, path) = self
            .open(&self.replay_dir, &format!("Replay_{now_ms}"))
            .map_err(|error| format!("cannot create the replay: {error}"))?;
        let start = self
            .ring
            .iter()
            .position(|packet| packet.is_keyframe() && packet.pts == from)
            .unwrap_or(0);
        for packet in self.ring.iter().skip(start) {
            writer
                .write(packet)
                .map_err(|error| format!("failed to write the replay: {error}"))?;
        }
        self.replay = Some((writer, path, id));
        Ok(())
    }

    fn open(&self, dir: &Path, stem: &str) -> io::Result<(Writer, PathBuf)> {
        Ok(match self.codec {
            VideoCodec::Av1 => {
                let path = dir.join(format!("{stem}.mkv"));
                let writer = MkvWriter::create(&path, self.size, self.audio)?;
                (Writer::Mkv(writer), path)
            }
            codec => {
                let path = dir.join(format!("{stem}.ts"));
                (
                    Writer::Ts(TsWriter::create(&path, codec, self.audio)?),
                    path,
                )
            }
        })
    }

    /// Keep the packet, then drop whole GOPs once the next one alone covers
    /// the buffer length.
    fn remember(&mut self, packet: Packet) {
        let newest = packet.pts;
        if packet.is_keyframe() {
            self.keys.push_back(packet.pts);
        }
        self.ring.push_back(packet);
        while self.keys.len() >= 2 && self.keys[1] <= newest - self.keep {
            self.keys.pop_front();
            while self
                .ring
                .front()
                .is_some_and(|front| !(front.is_keyframe() && front.pts == self.keys[0]))
            {
                self.ring.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SECOND: i64 = 10_000_000;

    fn dirs(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("wr-capture-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["replay", "regular"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        (root.join("replay"), root.join("regular"))
    }

    /// One frame per 100 ms, a keyframe every second.
    fn frame(index: i64) -> Packet {
        Packet {
            video: true,
            key: index % 10 == 0,
            pts: index * SECOND / 10,
            data: vec![index as u8; 32],
        }
    }

    fn request(name: &str, id: i64, seconds: Option<i64>) -> Value {
        let mut request = json!({ "id": id, "name": name });
        if let Some(seconds) = seconds {
            request["data"] = json!({ "seconds": seconds });
        }
        request
    }

    #[test]
    fn buffer_keeps_whole_gops_covering_its_length() {
        let (replay, regular) = dirs("ring");
        let mut router = Router::new(VideoCodec::H264, (64, 64), false, 3, replay, regular);
        for index in 0..100 {
            router.push(frame(index), 0);
        }
        // Newest is 9.9 s; 3 s back is 6.9 s, so the GOP from 6 s stays.
        assert_eq!(router.ring.front().unwrap().pts, 6 * SECOND);
        assert_eq!(
            router.keys,
            [6 * SECOND, 7 * SECOND, 8 * SECOND, 9 * SECOND]
        );
    }

    #[test]
    fn save_then_start_cuts_both_files_at_one_keyframe() {
        let (replay, regular) = dirs("cut");
        let mut router = Router::new(VideoCodec::H264, (64, 64), false, 30, replay, regular);
        for index in 0..55 {
            router.push(frame(index), 0);
        }
        // 2 s before 5.4 s is 3.4 s: the replay starts at the 3 s keyframe.
        assert!(
            router
                .request(&request("save-replay", 1, Some(2)), 7)
                .is_empty()
        );
        assert_eq!(
            router.request(&request("start-replay-recording", 2, None), 7)[0]["result"],
            "ok"
        );
        assert!(router.want_keyframe);
        let refused = router.request(&request("stop-replay-recording", 3, None), 7);
        assert_eq!(refused[0]["result"], "error");

        let mut replies = Vec::new();
        for index in 55..80 {
            replies.extend(router.push(frame(index), 9));
        }
        assert_eq!(replies.len(), 1, "the replay closes on the 6 s keyframe");
        assert_eq!(replies[0]["id"], 1);
        let replay_path = PathBuf::from(replies[0]["data"].as_str().unwrap());
        let stopped = router.request(&request("stop-replay-recording", 4, None), 9);
        assert_eq!(stopped[0]["result"], "ok");
        let regular_path = PathBuf::from(stopped[0]["data"].as_str().unwrap());

        // Each small frame is one TS packet on the video PID: replay frames
        // 30..=59 and regular frames 60..=79, nothing twice.
        let frames = |path: &Path| {
            let bytes = std::fs::read(path).unwrap();
            bytes
                .chunks(188)
                .filter(|chunk| chunk[1] & 0x1F == 0x01 && chunk[2] == 0x00)
                .count()
        };
        assert_eq!(frames(&replay_path), 30);
        assert_eq!(frames(&regular_path), 20);
        assert!(router.regular.is_none() && router.replay.is_none());
    }

    #[test]
    fn save_without_video_reports_the_missing_keyframe() {
        let (replay, regular) = dirs("empty");
        let mut router = Router::new(VideoCodec::H264, (64, 64), false, 30, replay, regular);
        let replies = router.request(&request("save-replay", 1, Some(5)), 0);
        assert_eq!(replies[0]["result"], "error");
    }
}
