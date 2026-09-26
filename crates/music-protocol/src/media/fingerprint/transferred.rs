//! Codec-tagged identity of the actual transferred file. No receiver timing,
//! gapless interpretation, source metadata, or native write authority lives here.
use super::{EncodedAlac, SourceFingerprint};
use crate::media::format::Format;
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq)]
pub struct TransferredAudio {
    pub sha256: [u8; 32],
    codec: Codec,
}

#[derive(Clone, PartialEq, Eq)]
enum Codec {
    Alac(EncodedAlac),
    Encoded(EncodedAudio),
}

/// The coded audio of every format other than ALAC and MPEG-1 Layer III,
/// which have their own identities. The digest covers the
/// codec configuration and the coded payload, never tags, pictures, padding or
/// where the payload sits, so retagging a file keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedAudio {
    pub format: Format,
    pub sample_rate: u32,
    pub channels: u8,
    /// PCM bits per sample of a lossless format; 0 for a lossy one.
    pub bits_per_sample: u8,
    /// Coded packets: MP4 samples, MPEG frames, PCM sample frames; 0 where
    /// counting them would need a decode (FLAC).
    pub packets: u64,
    /// Samples per channel the packets code, before any encoder trim.
    pub coded_samples: u64,
    /// Where the payload starts; outside the digest.
    pub audio_offset: u64,
    pub audio_bytes: u64,
    /// The codec configuration: an MP4 sample description with its sample
    /// sizes and times, FLAC STREAMINFO, a WAV `fmt ` or AIFF `COMM` chunk, an
    /// MPEG stream's first header.
    pub config_sha256: [u8; 32],
    pub audio_sha256: [u8; 32],
}

impl EncodedAudio {
    pub fn sha256(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"music-protocol/encoded-audio\0");
        h.update([self.format.code(), self.channels, self.bits_per_sample]);
        h.update(self.sample_rate.to_be_bytes());
        h.update(self.packets.to_be_bytes());
        h.update(self.coded_samples.to_be_bytes());
        h.update(self.audio_bytes.to_be_bytes());
        h.update(self.config_sha256);
        h.update(self.audio_sha256);
        h.finalize().into()
    }

    /// Fixed-width big-endian bytes for journal records.
    pub const BYTES: usize = 103;
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::BYTES);
        b.extend([self.format.code(), self.channels, self.bits_per_sample]);
        b.extend(self.sample_rate.to_be_bytes());
        for n in [
            self.packets,
            self.coded_samples,
            self.audio_offset,
            self.audio_bytes,
        ] {
            b.extend(n.to_be_bytes());
        }
        b.extend(self.config_sha256);
        b.extend(self.audio_sha256);
        b
    }
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        ensure!(
            b.len() == Self::BYTES,
            "invalid encoded audio record length"
        );
        let u64_at = |at: usize| u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
        Ok(Self {
            format: Format::from_code(b[0])
                .ok_or_else(|| anyhow::anyhow!("unknown audio format code {}", b[0]))?,
            channels: b[1],
            bits_per_sample: b[2],
            sample_rate: u32::from_be_bytes(b[3..7].try_into()?),
            packets: u64_at(7),
            coded_samples: u64_at(15),
            audio_offset: u64_at(23),
            audio_bytes: u64_at(31),
            config_sha256: b[39..71].try_into()?,
            audio_sha256: b[71..103].try_into()?,
        })
    }
}
