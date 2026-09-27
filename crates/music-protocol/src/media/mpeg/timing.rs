//! Source timing declarations, not decoder output or Apple transport values.
//! The complete bounded frame scan is hash-bound to EncodedMpeg1Layer3. Fractions
//! retain sample counts/rate exactly; no rounded duration enters identity.
use anyhow::{Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
};

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FRAMES: u32 = 2_000_000;
const SAMPLES: u64 = 1152;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SampleDuration {
    pub samples: u64,
    pub sample_rate: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BitrateCount {
    pub kbps: u16,
    pub frames: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BitrateMode {
    Constant,
    Variable,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FrameSummary {
    pub frame_count: u32,
    pub coded_duration: SampleDuration,
    pub bitrate_mode: BitrateMode,
    pub bitrates: Vec<BitrateCount>,
    /// Stereo, joint stereo, dual channel, mono respectively.
    pub channel_mode_counts: [u32; 4],
    pub crc_protected_frames: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeekHeader {
    Xing(XingInfo),
    Info(XingInfo),
    Vbri(Vbri),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct XingInfo {
    pub frame_offset: u16,
    pub flags: u32,
    /// This declaration excludes the leading Xing/Info frame; checked against
    /// the actual scan, when present. Missing is unknown, never zero.
    pub declared_audio_frames: Option<u32>,
    pub declared_stream_bytes: Option<u32>,
    pub toc: Option<Vec<u8>>,
    pub quality: Option<u32>,
    pub encoder: Option<EncoderTag>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EncoderTag {
    pub family: &'static str,
    pub identifier_hex: String,
    pub revision: u8,
    pub method: u8,
    pub delay_samples: u16,
    pub padding_samples: u16,
    /// Source tag arithmetic only, not decoded PCM or native gapless duration.
    pub declared_trimmed_duration: SampleDuration,
    pub raw_tag_sha256: String,
    pub crc_verified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VbriCountBasis {
    PhysicalFrames,
    ExcludingFirstFrame,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Vbri {
    pub frame_offset: u16,
    pub version: u16,
    pub delay_field: u16,
    pub quality: u16,
    pub declared_stream_bytes: u32,
    pub declared_frames: u32,
    pub count_basis: VbriCountBasis,
    pub table_entries: u16,
    pub table_scale: u16,
    pub bytes_per_entry: u16,
    pub frames_per_entry: u16,
    pub table_sha256: String,
    /// Scaled source declarations; aggregate coverage is checked, not the
    /// correspondence of each seek group to actual scanned frame offsets.
    pub table_byte_extents: Vec<u64>,
    pub table_offsets_verified: bool,
}
