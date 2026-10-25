// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal MPEG-TS writer: one H.264/HEVC stream and an optional ADTS AAC
//! stream. TS takes Annex-B video and ADTS audio exactly as the encoders emit
//! them, needs no index, and stays readable when the process dies mid-file.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::packet::{Packet, VideoCodec};

const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
const AUDIO_PID: u16 = 0x101;
/// Keeps every timestamp positive and leaves PCR room ahead of the PTS.
const PTS_OFFSET: i64 = 18_000;
const PCR_LEAD: i64 = 9_000;

pub struct TsWriter<W: Write> {
    out: W,
    codec: VideoCodec,
    audio: bool,
    /// Packet time of the file's first keyframe; everything is relative to it.
    base: Option<i64>,
    /// Continuity counters for the PAT, PMT, video and audio PIDs.
    cc: [u8; 4],
}

impl TsWriter<BufWriter<File>> {
    pub fn create(path: &Path, codec: VideoCodec, audio: bool) -> io::Result<Self> {
        Ok(Self::new(BufWriter::new(File::create(path)?), codec, audio))
    }
}

impl<W: Write> TsWriter<W> {
    pub fn new(out: W, codec: VideoCodec, audio: bool) -> Self {
        Self {
            out,
            codec,
            audio,
            base: None,
            cc: [0; 4],
        }
    }

    /// Write one packet. The file starts at the first video keyframe; earlier
    /// packets are dropped, so every file decodes from its first frame.
    pub fn write(&mut self, packet: &Packet) -> io::Result<()> {
        let base = match self.base {
            Some(base) => base,
            None if packet.is_keyframe() => *self.base.insert(packet.pts),
            None => return Ok(()),
        };
        if packet.pts < base || (!packet.video && !self.audio) {
            return Ok(());
        }
        // 100 ns units to the 90 kHz clock.
        let pts = (packet.pts - base) * 9 / 1_000 + PTS_OFFSET;
        if packet.is_keyframe() {
            self.write_tables()?;
        }
        let (pid, stream_id) = if packet.video {
            (VIDEO_PID, 0xE0)
        } else {
            (AUDIO_PID, 0xC0)
        };
        let mut pes = Vec::with_capacity(packet.data.len() + 14);
        pes.extend_from_slice(&[0, 0, 1, stream_id]);
        // Video may exceed the 16-bit length; zero means unbounded there.
        let length = packet.data.len() + 8;
        let length = if packet.video || length > 0xFFFF {
            0
        } else {
            length
        };
        pes.extend_from_slice(&(length as u16).to_be_bytes());
        pes.extend_from_slice(&[0x80, 0x80, 5]);
        pes.extend_from_slice(&timestamp(pts));
        pes.extend_from_slice(&packet.data);
        let pcr = packet.video.then_some(pts - PCR_LEAD);
        self.write_payload(pid, &pes, pcr, packet.is_keyframe())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush()
    }

    fn write_tables(&mut self) -> io::Result<()> {
        // PAT: program 1 lives on PMT_PID.
        let mut pat = vec![0x00, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01];
        pat.extend_from_slice(&(0xE000 | PMT_PID).to_be_bytes());
        self.write_section(0, pat)?;

        let video_type = match self.codec {
            VideoCodec::H264 => 0x1B,
            VideoCodec::Hevc => 0x24,
            VideoCodec::Av1 => unreachable!("AV1 is written as Matroska"),
        };
        let mut pmt = vec![0x02, 0xB0, 0, 0x00, 0x01, 0xC1, 0x00, 0x00];
        pmt.extend_from_slice(&(0xE000 | VIDEO_PID).to_be_bytes());
        pmt.extend_from_slice(&[0xF0, 0x00]);
        pmt.push(video_type);
        pmt.extend_from_slice(&(0xE000 | VIDEO_PID).to_be_bytes());
        pmt.extend_from_slice(&[0xF0, 0x00]);
        if self.audio {
            pmt.push(0x0F);
            pmt.extend_from_slice(&(0xE000 | AUDIO_PID).to_be_bytes());
            pmt.extend_from_slice(&[0xF0, 0x00]);
        }
        self.write_section(PMT_PID, pmt)
    }

    /// Fill in the section length, append the CRC and send it as one packet.
    fn write_section(&mut self, pid: u16, mut section: Vec<u8>) -> io::Result<()> {
        let length = section.len() - 3 + 4;
        section[1] = 0xB0 | (length >> 8) as u8;
        section[2] = length as u8;
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        let mut packet = [0xFF; 188];
        packet[..4].copy_from_slice(&[
            0x47,
            0x40 | (pid >> 8) as u8,
            pid as u8,
            0x10 | self.next_cc(pid),
        ]);
        packet[4] = 0; // pointer field
        packet[5..5 + section.len()].copy_from_slice(&section);
        self.out.write_all(&packet)
    }

    fn write_payload(
        &mut self,
        pid: u16,
        mut rest: &[u8],
        pcr: Option<i64>,
        random_access: bool,
    ) -> io::Result<()> {
        let mut first = true;
        while !rest.is_empty() {
            // Adaptation field body (everything after its length byte).
            let mut adaptation = Vec::new();
            if first && (pcr.is_some() || random_access) {
                adaptation.push(
                    if random_access { 0x40 } else { 0 } | if pcr.is_some() { 0x10 } else { 0 },
                );
                if let Some(pcr) = pcr {
                    let pcr = pcr.max(0) as u64;
                    adaptation.extend_from_slice(&[
                        (pcr >> 25) as u8,
                        (pcr >> 17) as u8,
                        (pcr >> 9) as u8,
                        (pcr >> 1) as u8,
                        ((pcr & 1) << 7) as u8 | 0x7E,
                        0,
                    ]);
                }
            }
            let mut has_adaptation = !adaptation.is_empty();
            let room = 184
                - if has_adaptation {
                    1 + adaptation.len()
                } else {
                    0
                };
            let take = rest.len().min(room);
            let stuffing = room - take;
            if stuffing > 0 {
                if has_adaptation {
                    adaptation.resize(adaptation.len() + stuffing, 0xFF);
                } else {
                    // The length byte itself is one byte of stuffing.
                    has_adaptation = true;
                    if stuffing >= 2 {
                        adaptation.push(0);
                        adaptation.resize(stuffing - 1, 0xFF);
                    }
                }
            }
            let mut packet = Vec::with_capacity(188);
            packet.extend_from_slice(&[
                0x47,
                if first { 0x40 } else { 0 } | (pid >> 8) as u8,
                pid as u8,
                if has_adaptation { 0x30 } else { 0x10 } | self.next_cc(pid),
            ]);
            if has_adaptation {
                packet.push(adaptation.len() as u8);
                packet.extend_from_slice(&adaptation);
            }
            packet.extend_from_slice(&rest[..take]);
            debug_assert_eq!(packet.len(), 188);
            self.out.write_all(&packet)?;
            rest = &rest[take..];
            first = false;
        }
        Ok(())
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let slot = match pid {
            0 => 0,
            PMT_PID => 1,
            VIDEO_PID => 2,
            _ => 3,
        };
        let value = self.cc[slot];
        self.cc[slot] = (value + 1) & 0x0F;
        value
    }
}

fn timestamp(pts: i64) -> [u8; 5] {
    let pts = pts as u64 & 0x1_FFFF_FFFF;
    [
        0x21 | ((pts >> 29) & 0x0E) as u8,
        (pts >> 22) as u8,
        ((pts >> 14) & 0xFE) as u8 | 1,
        (pts >> 7) as u8,
        ((pts << 1) & 0xFE) as u8 | 1,
    ]
}

/// MPEG-2 CRC-32 (polynomial 0x04C11DB7, no reflection).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(video: bool, key: bool, pts: i64, len: usize) -> Packet {
        Packet {
            video,
            key,
            pts,
            data: vec![0xAB; len],
        }
    }

    #[test]
    fn output_is_whole_ts_packets_starting_at_the_first_keyframe() {
        let mut out = Vec::new();
        let mut writer = TsWriter::new(&mut out, VideoCodec::H264, true);
        writer.write(&packet(true, false, 0, 500)).unwrap();
        assert!(out.is_empty(), "nothing before the first keyframe");
        let mut writer = TsWriter::new(&mut out, VideoCodec::H264, true);
        for (index, len) in [1, 183, 184, 185, 4000].into_iter().enumerate() {
            writer
                .write(&packet(true, index == 0, index as i64 * 166_666, len))
                .unwrap();
            writer
                .write(&packet(false, false, index as i64 * 213_333, len))
                .unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(out.len() % 188, 0);
        assert!(out.chunks(188).all(|chunk| chunk[0] == 0x47));
    }

    #[test]
    fn crc_matches_the_mpeg2_reference() {
        // The standard PAT for program 1 on PID 0x1000 with TSID 1.
        let pat = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        assert_eq!(crc32(&pat), 0x2AB104B2);
    }
}
