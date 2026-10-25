// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal Matroska writer for AV1 video and AAC audio. AV1 has no MPEG-TS
//! mapping FFmpeg can read back, so AV1 files use Matroska, as GSR's do on
//! Linux. Each video keyframe starts a cluster; a cluster is buffered and
//! written whole, so a crash loses at most the last GOP.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::packet::Packet;

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// Block times are signed 16-bit offsets from the cluster's time.
const MAX_CLUSTER_MS: i64 = 30_000;

pub struct MkvWriter<W: Write> {
    out: W,
    size: (u32, u32),
    audio: bool,
    /// Packet time of the file's first keyframe; everything is relative to it.
    base: Option<i64>,
    /// The open cluster's time in ms and its encoded blocks.
    cluster: Option<(i64, Vec<u8>)>,
}

impl MkvWriter<BufWriter<File>> {
    pub fn create(path: &Path, size: (u32, u32), audio: bool) -> io::Result<Self> {
        Ok(Self::new(BufWriter::new(File::create(path)?), size, audio))
    }
}

impl<W: Write> MkvWriter<W> {
    pub fn new(out: W, size: (u32, u32), audio: bool) -> Self {
        Self {
            out,
            size,
            audio,
            base: None,
            cluster: None,
        }
    }

    /// Write one packet. The file starts at the first video keyframe, whose
    /// sequence header goes into the track's codec private data.
    pub fn write(&mut self, packet: &Packet) -> io::Result<()> {
        let base = match self.base {
            Some(base) => base,
            None if packet.is_keyframe() => {
                self.write_header(&packet.data)?;
                *self.base.insert(packet.pts)
            }
            None => return Ok(()),
        };
        if packet.pts < base || (!packet.video && !self.audio) {
            return Ok(());
        }
        let ms = (packet.pts - base) / 10_000;
        let new_cluster = match &self.cluster {
            None => true,
            Some((start, _)) => packet.is_keyframe() || ms - start >= MAX_CLUSTER_MS,
        };
        if new_cluster {
            self.flush_cluster()?;
            self.cluster = Some((ms, Vec::new()));
        }
        let (start, blocks) = self.cluster.as_mut().expect("opened above");
        let (track, payload) = if packet.video {
            (1, strip_temporal_delimiters(&packet.data))
        } else {
            (2, strip_adts(&packet.data).to_vec())
        };
        let relative = (ms - *start).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
        let flags = if packet.is_keyframe() || !packet.video {
            0x80
        } else {
            0
        };
        let mut block = vec![0x80 | track];
        block.extend_from_slice(&relative.to_be_bytes());
        block.push(flags);
        block.extend_from_slice(&payload);
        blocks.extend(element(0xA3, &block));
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.flush_cluster()?;
        self.out.flush()
    }

    fn flush_cluster(&mut self) -> io::Result<()> {
        if let Some((start, blocks)) = self.cluster.take() {
            let mut body = uint(0xE7, start as u64);
            body.extend(blocks);
            self.out.write_all(&element(0x1F43_B675, &body))?;
        }
        Ok(())
    }

    fn write_header(&mut self, keyframe: &[u8]) -> io::Result<()> {
        let ebml = [
            uint(0x4286, 1),
            uint(0x42F7, 1),
            uint(0x42F2, 4),
            uint(0x42F3, 8),
            element(0x4282, b"matroska"),
            uint(0x4287, 4),
            uint(0x4285, 2),
        ]
        .concat();
        self.out.write_all(&element(0x1A45_DFA3, &ebml))?;
        // Segment of unknown size: the file is written front to back.
        self.out.write_all(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        ])?;
        let info = [
            uint(0x2A_D7B1, 1_000_000),
            element(0x4D80, b"wr-capture"),
            element(0x5741, b"wr-capture"),
        ]
        .concat();
        self.out.write_all(&element(0x1549_A966, &info))?;

        let (width, height) = self.size;
        let video = [
            uint(0xD7, 1),
            uint(0x73C5, 1),
            uint(0x83, 1),
            element(0x86, b"V_AV1"),
            element(0x63A2, &av1_config(keyframe)),
            element(
                0xE0,
                &[uint(0xB0, width.into()), uint(0xBA, height.into())].concat(),
            ),
        ]
        .concat();
        let mut tracks = element(0xAE, &video);
        if self.audio {
            let audio = [
                uint(0xD7, 2),
                uint(0x73C5, 2),
                uint(0x83, 2),
                element(0x86, b"A_AAC"),
                // AudioSpecificConfig: AAC-LC, 48 kHz, stereo.
                element(0x63A2, &[0x11, 0x90]),
                element(
                    0xE1,
                    &[element(0xB5, &48_000f64.to_be_bytes()), uint(0x9F, 2)].concat(),
                ),
            ]
            .concat();
            tracks.extend(element(0xAE, &audio));
        }
        self.out.write_all(&element(0x1654_AE6B, &tracks))
    }
}

/// One EBML element: its ID (already carrying its length marker) and body.
fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().take_while(|&&byte| byte == 0).count();
    let mut out = id_bytes[skip..].to_vec();
    let len = body.len() as u64;
    let width = (1..=8)
        .find(|width| len < (1u64 << (7 * width)) - 1)
        .expect("element fits in 56 bits");
    let marked = (1u64 << (7 * width)) | len;
    out.extend_from_slice(&marked.to_be_bytes()[8 - width as usize..]);
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count().min(7);
    element(id, &bytes[skip..])
}

/// The OBUs of an AV1 temporal unit as (type, whole OBU bytes).
fn obus(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let header = *rest.first()?;
        let mut at = 1 + usize::from(header & 0x04 != 0);
        let size = if header & 0x02 != 0 {
            let mut size = 0usize;
            for shift in 0..8 {
                let byte = *rest.get(at)?;
                at += 1;
                size |= usize::from(byte & 0x7F) << (7 * shift);
                if byte & 0x80 == 0 {
                    break;
                }
            }
            size
        } else {
            rest.len().checked_sub(at)?
        };
        let end = (at + size).min(rest.len());
        let (obu, tail) = rest.split_at(end);
        rest = tail;
        Some(((header >> 3) & 0x0F, obu))
    })
}

pub fn has_sequence_header(data: &[u8]) -> bool {
    obus(data).any(|(kind, _)| kind == OBU_SEQUENCE_HEADER)
}

/// Matroska blocks carry AV1 without temporal delimiters.
fn strip_temporal_delimiters(data: &[u8]) -> Vec<u8> {
    obus(data)
        .filter(|(kind, _)| *kind != OBU_TEMPORAL_DELIMITER)
        .flat_map(|(_, obu)| obu.iter().copied())
        .collect()
}

/// `av1C` with the keyframe's sequence header. Hardware encoders here emit
/// 8-bit 4:2:0 from NV12; the level is left at 4.0, and FFmpeg re-derives the
/// exact record from the sequence header when it writes MP4.
fn av1_config(keyframe: &[u8]) -> Vec<u8> {
    let header = obus(keyframe)
        .find(|(kind, _)| *kind == OBU_SEQUENCE_HEADER)
        .map_or(&[][..], |(_, obu)| obu);
    let profile = obus(header)
        .next()
        .and_then(|(_, obu)| obu.get(obu.len() - payload_len(obu)))
        .map_or(0, |first| first >> 5);
    let mut config = vec![0x81, (profile << 5) | 8, 0x0C, 0];
    config.extend_from_slice(header);
    config
}

/// Length of an OBU's payload, after its header and size field.
fn payload_len(obu: &[u8]) -> usize {
    let mut at = 1 + usize::from(obu[0] & 0x04 != 0);
    if obu[0] & 0x02 != 0 {
        while obu.get(at).is_some_and(|byte| byte & 0x80 != 0) {
            at += 1;
        }
        at += 1;
    }
    obu.len().saturating_sub(at)
}

/// ADTS frames lose their 7- or 9-byte header; the track config replaces it.
fn strip_adts(data: &[u8]) -> &[u8] {
    if data.len() > 9 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0 {
        let header = if data[1] & 0x01 != 0 { 7 } else { 9 };
        &data[header..]
    } else {
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_sizes_use_the_shortest_vint() {
        assert_eq!(element(0xA3, &[1, 2]), [0xA3, 0x82, 1, 2]);
        assert_eq!(element(0x86, &[0; 200])[..3], [0x86, 0x40, 200]);
        assert_eq!(uint(0xD7, 1), [0xD7, 0x81, 1]);
    }

    #[test]
    fn temporal_delimiters_are_dropped() {
        // TD, then a sequence header with a 2-byte payload.
        let unit = [0x12, 0x00, 0x0A, 0x02, 0x20, 0x00];
        assert_eq!(strip_temporal_delimiters(&unit), [0x0A, 0x02, 0x20, 0x00]);
        assert_eq!(av1_config(&unit)[1], (1 << 5) | 8);
    }
}
