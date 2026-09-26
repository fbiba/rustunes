//! Structural readers for the containers of every format other than ALAC and
//! MPEG-1 Layer III: MP4 (AAC, HE-AAC, xHE-AAC), FLAC, WAV, AIFF and MPEG-2 /
//! 2.5 Layer III. Each finds the coded payload and its configuration, hashes
//! both into an [`EncodedAudio`], and reports the stream facts an insert
//! needs and where the tags are. Nothing is decoded.
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::media::{fingerprint::EncodedAudio, format::Format};

pub mod aiff;
pub mod flac;
pub mod mp4;

pub(crate) const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Tag data a reader may hold in memory (pictures included).
pub(crate) const MAX_TAG_BYTES: u64 = 16 * 1024 * 1024;

/// What a container holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub encoded: EncodedAudio,
    pub stream: Stream,
    /// Where the tags are, in file order.
    pub tags: Vec<TagRegion>,
}

/// Facts about the coded stream that an insert carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    /// Average kb/s over the coded payload, or the nominal rate the stream
    /// declares (MP4 `esds`, constant-rate MPEG).
    pub bit_rate_kbps: u32,
    /// The bit rate varies from packet to packet.
    pub variable_bit_rate: bool,
    /// Samples the encoder put before and after the audio, as the container
    /// declares them (MP4 edit list); 0 where it declares none.
    pub encoder_delay: u64,
    pub encoder_padding: u64,
}

impl Stream {
    /// Samples a player plays: the coded ones less the encoder's.
    pub fn valid_samples(&self, coded_samples: u64) -> Result<u64> {
        coded_samples
            .checked_sub(self.encoder_delay)
            .and_then(|n| n.checked_sub(self.encoder_padding))
            .filter(|n| *n > 0)
            .context("encoder delay and padding exceed the coded samples")
    }
}

/// A span of tag data and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagRegion {
    pub kind: TagKind,
    pub offset: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagKind {
    /// A complete ID3v2 tag (MPEG prefix, WAV `id3 ` chunk, AIFF `ID3 ` chunk).
    Id3v2,
    /// A trailing 128-byte ID3v1 tag.
    Id3v1,
    /// A RIFF `LIST` chunk's payload starting with `INFO`.
    RiffInfo,
    /// An AIFF text chunk (`NAME`, `AUTH`, `(c) `, `ANNO`); the id is in
    /// [`TagRegion::offset`] - 8.
    AiffText,
    /// A FLAC `VORBIS_COMMENT` block's payload.
    VorbisComment,
    /// A FLAC `PICTURE` block's payload.
    FlacPicture,
    /// An MP4 `moov` (its `udta/meta/ilst`).
    Mp4Movie,
}

/// SHA-256 of `bytes` bytes at `offset`.
pub(crate) fn hash_span(
    input: &mut (impl Read + Seek),
    offset: u64,
    bytes: u64,
) -> Result<[u8; 32]> {
    input.seek(SeekFrom::Start(offset))?;
    let mut left = bytes;
    let mut buffer = vec![0; 64 * 1024];
    let mut digest = Sha256::new();
    while left > 0 {
        let count = usize::try_from(left.min(buffer.len() as u64))?;
        input
            .read_exact(&mut buffer[..count])
            .context("audio payload truncated")?;
        digest.update(&buffer[..count]);
        left -= count as u64;
    }
    Ok(digest.finalize().into())
}

/// Read `bytes` bytes at `offset`, within the tag bound.
pub(crate) fn read_span(
    input: &mut (impl Read + Seek),
    offset: u64,
    bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(bytes <= MAX_TAG_BYTES, "metadata span exceeds bound");
    input.seek(SeekFrom::Start(offset))?;
    let mut out = vec![0; usize::try_from(bytes)?];
    input
        .read_exact(&mut out)
        .context("metadata span truncated")?;
    Ok(out)
}

/// Average kb/s of `bytes` coded bytes playing for `samples` at `rate`.
pub(crate) fn average_kbps(bytes: u64, samples: u64, rate: u32) -> Result<u32> {
    ensure!(samples > 0 && rate > 0, "empty audio duration");
    let bits = u128::from(bytes) * 8 * u128::from(rate);
    let per_second = bits / u128::from(samples);
    Ok(u32::try_from((per_second + 500) / 1000)?)
}

/// One chunk of a RIFF- or IFF-style file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chunk {
    pub id: [u8; 4],
    pub offset: u64,
    pub bytes: u64,
}

/// The chunks of a RIFF (little-endian sizes) or IFF (big-endian) file whose
/// form header declares the whole file, padded to even lengths.
pub(crate) fn chunks(
    input: &mut (impl Read + Seek),
    size: u64,
    little_endian: bool,
) -> Result<Vec<Chunk>> {
    input.seek(SeekFrom::Start(4))?;
    let mut word = [0; 4];
    input.read_exact(&mut word)?;
    let declared = u64::from(if little_endian {
        u32::from_le_bytes(word)
    } else {
        u32::from_be_bytes(word)
    });
    ensure!(
        declared + 8 == size,
        "the container's declared length ({}) differs from the file's ({size})",
        declared + 8
    );
    let mut position = 12;
    let mut out = Vec::new();
    while position < size {
        ensure!(out.len() < 1024, "too many chunks");
        ensure!(size - position >= 8, "truncated chunk header");
        input.seek(SeekFrom::Start(position))?;
        let mut header = [0; 8];
        input.read_exact(&mut header)?;
        let id: [u8; 4] = header[..4].try_into()?;
        ensure!(
            id.iter().all(|b| (0x20..0x7f).contains(b)),
            "invalid chunk id"
        );
        let length: [u8; 4] = header[4..].try_into()?;
        let bytes = u64::from(if little_endian {
            u32::from_le_bytes(length)
        } else {
            u32::from_be_bytes(length)
        });
        let end = position + 8 + bytes;
        let padded = end + (bytes & 1);
        // A writer may leave out the final pad byte.
        ensure!(
            padded <= size || (end == size && bytes & 1 == 1),
            "chunk {} exceeds the file",
            String::from_utf8_lossy(&id)
        );
        out.push(Chunk {
            id,
            offset: position + 8,
            bytes,
        });
        position = padded.min(size);
    }
    Ok(out)
}

/// The sole chunk with this id.
pub(crate) fn sole(chunks: &[Chunk], id: &[u8; 4]) -> Result<Chunk> {
    let mut found = chunks.iter().filter(|c| &c.id == id);
    let Some(chunk) = found.next() else {
        bail!("missing {} chunk", String::from_utf8_lossy(id));
    };
    ensure!(
        found.next().is_none(),
        "more than one {} chunk",
        String::from_utf8_lossy(id)
    );
    Ok(*chunk)
}

/// The identity of a PCM payload: `config` is the format chunk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pcm_layout(
    input: &mut (impl Read + Seek),
    format: Format,
    config: &[u8],
    sample_rate: u32,
    channels: u8,
    bits_per_sample: u8,
    frames: u64,
    audio_offset: u64,
    audio_bytes: u64,
    tags: Vec<TagRegion>,
) -> Result<Layout> {
    ensure!(frames > 0, "no audio samples");
    let encoded = EncodedAudio {
        format,
        sample_rate,
        channels,
        bits_per_sample,
        packets: frames,
        coded_samples: frames,
        audio_offset,
        audio_bytes,
        config_sha256: Sha256::digest(config).into(),
        audio_sha256: hash_span(input, audio_offset, audio_bytes)?,
    };
    let bit_rate_kbps = u32::try_from(
        (u64::from(sample_rate) * u64::from(channels) * u64::from(bits_per_sample) + 500) / 1000,
    )?;
    Ok(Layout {
        encoded,
        stream: Stream {
            bit_rate_kbps,
            variable_bit_rate: false,
            encoder_delay: 0,
            encoder_padding: 0,
        },
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, path::PathBuf};

    fn fixture(name: &str) -> (File, u64) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let file = File::open(path).unwrap();
        let size = file.metadata().unwrap().len();
        (file, size)
    }
}
