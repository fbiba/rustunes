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
impl MpegFrameReport {
    pub(super) fn matches(&self, expected: &TransferredSource) -> bool {
        expected.audio.mpeg().is_some_and(|e| {
            self.sample_rate == e.sample_rate
                && self.channels == e.channels
                && self.frame_count == e.frame_count
                && self.audio_offset == e.audio_offset
                && self.audio_bytes == e.audio_bytes
                && u64::from(self.trailing_tag_bytes) == e.trailing_bytes
                && self.frame_bytes_sha256 == hex(&e.frame_bytes_sha256)
                && self.encoded_audio_sha256 == hex(&expected.audio.sha256)
        })
    }
}

struct Controlled<'a> {
    bytes: Cursor<&'a [u8]>,
    control: &'a ReadControl,
}
impl Read for Controlled<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.control.check().map_err(std::io::Error::other)?;
        self.bytes.read(output)
    }
}
impl Seek for Controlled<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.control.check().map_err(std::io::Error::other)?;
        self.bytes.seek(position)
    }
}
fn hash(bytes: &[u8], control: &ReadControl) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        control.check()?;
        hash.update(chunk);
    }
    Ok(hash.finalize().into())
}

pub(super) fn inspect(bytes: &[u8], control: &ReadControl) -> Result<([u8; 32], MpegFrameReport)> {
    control.check()?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_AUDIO,
        "MPEG readback exceeds native transfer bound"
    );
    let OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
    } = inspect_mpeg_frames(
        &mut Controlled {
            bytes: Cursor::new(bytes),
            control,
        },
        bytes.len() as u64,
    )?
    else {
        anyhow::bail!("readback is not an MPEG-1 Layer III stream");
    };
    let start = usize::try_from(audio_offset)?;
    let end = start
        .checked_add(usize::try_from(audio_bytes)?)
        .ok_or_else(|| anyhow::anyhow!("MPEG span overflow"))?;
    let frame_bytes_sha256 = hash(
        bytes
            .get(start..end)
            .ok_or_else(|| anyhow::anyhow!("MPEG span outside file"))?,
        control,
    )?;
    let full_file_sha256 = hash(bytes, control)?;
    control.check()?;
    Ok((
        full_file_sha256,
        report(
            bytes.len() as u64,
            full_file_sha256,
            &OtherCodec::Mpeg1Layer3 {
                sample_rate,
                channels,
                frame_count,
                audio_offset,
                audio_bytes,
                trailing_tag_bytes,
            },
            frame_bytes_sha256,
        )?,
    ))
}

/// Frame facts measured on actual bytes: by this readback, or by an earlier
/// verified copy or upload of the same unchanged file. Never from the binding.
pub(super) fn report(
    file_bytes: u64,
    full_file_sha256: [u8; 32],
    codec: &OtherCodec,
    frame_bytes_sha256: [u8; 32],
) -> Result<MpegFrameReport> {
    let OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
    } = *codec
    else {
        anyhow::bail!("frame facts are for an MPEG-1 Layer III stream");
    };
    let source = TransferredSource::mpeg(
        file_bytes,
        full_file_sha256,
        EncodedMpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_bytes: u64::from(trailing_tag_bytes),
            frame_bytes_sha256,
        },
    )?;
    Ok(MpegFrameReport {
        format: "encoded-mpeg1-layer3",
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
        frame_bytes_sha256: hex(&frame_bytes_sha256),
        encoded_audio_sha256: hex(&source.audio.sha256),
        encoded_identity_verified: false,
    })
}
