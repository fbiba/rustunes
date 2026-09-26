//! Codec-tagged identity of the actual transferred file. No receiver timing,
//! gapless interpretation, source metadata, or native write authority lives here.
use super::{EncodedAlac, SourceFingerprint};
use crate::media::format::Format;
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq)]
pub struct TransferredSource {
    pub file_bytes: u64,
    pub full_file_sha256: [u8; 32],
    pub audio: TransferredAudio,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TransferredAudio {
    pub sha256: [u8; 32],
    codec: Codec,
}

#[derive(Clone, PartialEq, Eq)]
enum Codec {
    Alac(EncodedAlac),
    Mpeg1Layer3(EncodedMpeg1Layer3),
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

    pub fn validate(&self, file_bytes: u64) -> Result<()> {
        ensure!(
            !matches!(self.format, Format::Alac | Format::Mpeg1Layer3),
            "ALAC and MPEG-1 Layer III keep their own identities"
        );
        ensure!(
            (1..=768_000).contains(&self.sample_rate)
                && (1..=8).contains(&self.channels)
                && self.bits_per_sample <= 32
                && (self.bits_per_sample > 0) == self.format.lossless()
                && self.coded_samples > 0
                && (self.packets > 0 || self.format == Format::Flac)
                && self.config_sha256 != [0; 32]
                && self.audio_sha256 != [0; 32],
            "invalid encoded audio identity"
        );
        ensure!(
            self.audio_bytes > 0
                && self
                    .audio_offset
                    .checked_add(self.audio_bytes)
                    .is_some_and(|end| end <= file_bytes),
            "encoded audio lies outside its file"
        );
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EncodedMpeg1Layer3 {
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_count: u32,
    pub audio_offset: u64,
    pub audio_bytes: u64,
    pub trailing_bytes: u64,
    pub frame_bytes_sha256: [u8; 32],
}

impl EncodedMpeg1Layer3 {
    /// The same real-frame identity used by MP3 source inspection. Coded sample
    /// count is derived, never interpreted as native or gapless duration.
    pub fn sha256(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"music-protocol/encoded-mpeg1-layer3\0");
        h.update(self.sample_rate.to_le_bytes());
        h.update([self.channels]);
        h.update(self.frame_count.to_le_bytes());
        h.update(self.audio_bytes.to_le_bytes());
        h.update(self.frame_bytes_sha256);
        h.finalize().into()
    }

    fn validate(&self, file_bytes: u64) -> Result<()> {
        ensure!(
            matches!(self.sample_rate, 32000 | 44100 | 48000)
                && matches!(self.channels, 1 | 2)
                && (1..=2_000_000).contains(&self.frame_count)
                && self.frame_bytes_sha256 != [0; 32],
            "invalid MPEG frame identity"
        );
        let frames = u64::from(self.frame_count);
        ensure!(
            (frames * 96..=frames * 1441).contains(&self.audio_bytes)
                && self.audio_offset <= 16 * 1024 * 1024 + 20
                && matches!(self.trailing_bytes, 0 | 128)
                && self
                    .audio_offset
                    .checked_add(self.audio_bytes)
                    .and_then(|n| n.checked_add(self.trailing_bytes))
                    == Some(file_bytes),
            "invalid MPEG source layout"
        );
        Ok(())
    }
}

impl TransferredSource {
    pub fn mpeg(
        file_bytes: u64,
        full_file_sha256: [u8; 32],
        mpeg: EncodedMpeg1Layer3,
    ) -> Result<Self> {
        let value = Self {
            file_bytes,
            full_file_sha256,
            audio: TransferredAudio {
                sha256: mpeg.sha256(),
                codec: Codec::Mpeg1Layer3(mpeg),
            },
        };
        value.validate()?;
        Ok(value)
    }

    pub fn encoded(
        file_bytes: u64,
        full_file_sha256: [u8; 32],
        audio: EncodedAudio,
    ) -> Result<Self> {
        let value = Self {
            file_bytes,
            full_file_sha256,
            audio: TransferredAudio {
                sha256: audio.sha256(),
                codec: Codec::Encoded(audio),
            },
        };
        value.validate()?;
        Ok(value)
    }

    /// The format of the transferred audio.
    pub fn format(&self) -> Format {
        match &self.audio.codec {
            Codec::Alac(_) => Format::Alac,
            Codec::Mpeg1Layer3(_) => Format::Mpeg1Layer3,
            Codec::Encoded(a) => a.format,
        }
    }

    pub fn is_alac(&self) -> bool {
        matches!(self.audio.codec, Codec::Alac(_))
    }

    pub fn require_alac(&self) -> Result<SourceFingerprint> {
        Ok(SourceFingerprint {
            file_bytes: self.file_bytes,
            full_file_sha256: self.full_file_sha256,
            audio: self.audio.alac()?.clone(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=512 * 1024 * 1024).contains(&self.file_bytes) && self.full_file_sha256 != [0; 32],
            "invalid transferred source bounds"
        );
        match &self.audio.codec {
            Codec::Alac(a) => {
                ensure!(
                    a.media_bytes > 0
                        && a.media_bytes < self.file_bytes
                        && a.timescale > 0
                        && a.duration_ticks > 0,
                    "invalid ALAC content bounds"
                );
                ensure!(
                    a.composite_sha256() == a.sha256 && a.sha256 == self.audio.sha256,
                    "encoded fingerprint is inconsistent"
                );
            }
            Codec::Mpeg1Layer3(a) => {
                a.validate(self.file_bytes)?;
                ensure!(
                    a.sha256() == self.audio.sha256,
                    "encoded MPEG fingerprint is inconsistent"
                );
            }
            Codec::Encoded(a) => {
                a.validate(self.file_bytes)?;
                ensure!(
                    a.sha256() == self.audio.sha256,
                    "encoded audio fingerprint is inconsistent"
                );
            }
        }
        Ok(())
    }
}

impl TransferredAudio {
    pub fn alac(&self) -> Result<&EncodedAlac> {
        match &self.codec {
            Codec::Alac(a) => Ok(a),
            Codec::Mpeg1Layer3(_) | Codec::Encoded(_) => {
                bail!("operation requires qualified ALAC content; this is another codec")
            }
        }
    }
    pub fn mpeg(&self) -> Option<&EncodedMpeg1Layer3> {
        match &self.codec {
            Codec::Mpeg1Layer3(a) => Some(a),
            Codec::Alac(_) | Codec::Encoded(_) => None,
        }
    }
    pub fn encoded(&self) -> Option<&EncodedAudio> {
        match &self.codec {
            Codec::Encoded(a) => Some(a),
            Codec::Alac(_) | Codec::Mpeg1Layer3(_) => None,
        }
    }
}

impl From<SourceFingerprint> for TransferredSource {
    fn from(value: SourceFingerprint) -> Self {
        Self {
            file_bytes: value.file_bytes,
            full_file_sha256: value.full_file_sha256,
            audio: TransferredAudio {
                sha256: value.audio.sha256,
                codec: Codec::Alac(value.audio),
            },
        }
    }
}

impl PartialEq<SourceFingerprint> for TransferredSource {
    fn eq(&self, other: &SourceFingerprint) -> bool {
        self.file_bytes == other.file_bytes
            && self.full_file_sha256 == other.full_file_sha256
            && self.audio.sha256 == other.audio.sha256
            && matches!(&self.audio.codec, Codec::Alac(a) if a == &other.audio)
    }
}
impl PartialEq<TransferredSource> for SourceFingerprint {
    fn eq(&self, other: &TransferredSource) -> bool {
        other == self
    }
}

/// Lookup key only: querying by a digest grants neither ownership nor a permit.
pub trait EncodedIdentity {
    fn encoded_sha256(&self) -> &[u8; 32];
}
impl EncodedIdentity for EncodedAlac {
    fn encoded_sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}
impl EncodedIdentity for TransferredAudio {
    fn encoded_sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}
impl PartialEq<EncodedAlac> for TransferredAudio {
    fn eq(&self, other: &EncodedAlac) -> bool {
        self.sha256 == other.sha256 && matches!(&self.codec, Codec::Alac(a) if a == other)
    }
}
impl PartialEq<TransferredAudio> for EncodedAlac {
    fn eq(&self, other: &TransferredAudio) -> bool {
        other == self
    }
}
