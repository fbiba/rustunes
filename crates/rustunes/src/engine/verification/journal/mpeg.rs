//! Actual-byte MPEG verification only. No metadata projection, native timing,
//! journal transition or write capability is produced here.
use super::ReadControl;
use crate::engine::state::audio_index::{OtherCodec, inspect_mpeg_frames};
use anyhow::{Result, ensure};
use music_protocol::media::fingerprint::{EncodedMpeg1Layer3, TransferredSource};
use music_protocol::protocol::transfer::{MAX_AUDIO, hex};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Present only for an MPEG binding; ALAC reports omit the entire field.
/// Facts remain provisional until the enclosing batch's final state guard.
#[derive(Serialize)]
pub struct MpegFrameReport {
    format: &'static str,
    sample_rate: u32,
    channels: u8,
    frame_count: u32,
    audio_offset: u64,
    audio_bytes: u64,
    trailing_tag_bytes: u32,
    frame_bytes_sha256: String,
    encoded_audio_sha256: String,
    pub(super) encoded_identity_verified: bool,
}
