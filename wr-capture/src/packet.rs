// SPDX-License-Identifier: GPL-3.0-or-later

/// One encoded access unit: an Annex-B (H.264/HEVC) or OBU (AV1) video frame,
/// or an ADTS AAC frame.
#[derive(Clone, Debug)]
pub struct Packet {
    pub video: bool,
    pub key: bool,
    /// Presentation time in 100 ns units on the shared capture clock.
    pub pts: i64,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn is_keyframe(&self) -> bool {
        self.video && self.key
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}
