//! The audio formats the phone's music library takes, as Apple Devices names
//! them on the wire: the `avformat_info.audio_format` number, the
//! `location.kind` text, the extension of the file on the phone, and the limits
//! the phone itself advertises (Lockdown `com.apple.mobile.iTunes`,
//! `AudioCodecs`).
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Mpeg1Layer3,
    Mpeg2Layer3,
    Mpeg25Layer3,
    AacLc,
    HeAac,
    HeAacV2,
    /// xHE-AAC.
    Usac,
    Alac,
    Flac,
    /// Integer PCM in RIFF WAVE.
    Wav,
    /// Integer PCM in AIFF or AIFF-C (`NONE` or `sowt`).
    Aiff,
}

const ALL: [Format; 11] = [
    Format::Mpeg1Layer3,
    Format::Mpeg2Layer3,
    Format::Mpeg25Layer3,
    Format::AacLc,
    Format::HeAac,
    Format::HeAacV2,
    Format::Usac,
    Format::Alac,
    Format::Flac,
    Format::Wav,
    Format::Aiff,
];

impl Format {
    /// Stable number used in identities and journal records; never reuse one.
    pub const fn code(self) -> u8 {
        match self {
            Self::Mpeg1Layer3 => 1,
            Self::Mpeg2Layer3 => 2,
            Self::Mpeg25Layer3 => 3,
            Self::AacLc => 4,
            Self::HeAac => 5,
            Self::HeAacV2 => 6,
            Self::Usac => 7,
            Self::Alac => 8,
            Self::Flac => 9,
            Self::Wav => 10,
            Self::Aiff => 11,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        ALL.into_iter().find(|format| format.code() == code)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        ALL.into_iter().find(|format| format.name() == name)
    }

    /// The name reports and caches use.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mpeg1Layer3 => "mpeg1-layer3",
            Self::Mpeg2Layer3 => "mpeg2-layer3",
            Self::Mpeg25Layer3 => "mpeg25-layer3",
            Self::AacLc => "aac-lc",
            Self::HeAac => "he-aac",
            Self::HeAacV2 => "he-aac-v2",
            Self::Usac => "usac",
            Self::Alac => "alac",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
        }
    }

    /// The name a plan's codec fields use.
    pub const fn codec_label(self) -> &'static str {
        match self {
            Self::Mpeg1Layer3 => "mpeg1_layer3",
            Self::Mpeg2Layer3 => "mpeg2_layer3",
            Self::Mpeg25Layer3 => "mpeg25_layer3",
            Self::AacLc => "aac_lc",
            Self::HeAac => "he_aac",
            Self::HeAacV2 => "he_aac_v2",
            Self::Usac => "usac",
            Self::Alac => "alac",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
        }
    }

    /// Whether the phone file is MP4, whose cover's native checksum counts
    /// the whole `data` atom.
    pub const fn mp4(self) -> bool {
        matches!(
            self,
            Self::AacLc | Self::HeAac | Self::HeAacV2 | Self::Usac | Self::Alac
        )
    }

    /// What people call it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mpeg1Layer3 => "MPEG-1 Layer III",
            Self::Mpeg2Layer3 => "MPEG-2 Layer III",
            Self::Mpeg25Layer3 => "MPEG-2.5 Layer III",
            Self::AacLc => "AAC-LC",
            Self::HeAac => "HE-AAC",
            Self::HeAacV2 => "HE-AAC v2",
            Self::Usac => "xHE-AAC",
            Self::Alac => "ALAC",
            Self::Flac => "FLAC",
            Self::Wav => "WAV (PCM)",
            Self::Aiff => "AIFF (PCM)",
        }
    }

    /// Apple's `audio_format` number (AMPDevicesAgent `0x1403af144`).
    pub const fn audio_format(self) -> u16 {
        match self {
            Self::Mpeg1Layer3 => 301,
            Self::Mpeg2Layer3 => 302,
            Self::Mpeg25Layer3 => 303,
            Self::AacLc => 502,
            Self::HeAac => 508,
            Self::HeAacV2 => 509,
            Self::Usac => 1001,
            Self::Alac => 601,
            Self::Flac => 602,
            Self::Wav | Self::Aiff => 701,
        }
    }

    /// The library kind Apple Devices copies into `location.kind`. Only the
    /// MPEG and Apple Lossless texts were seen in Apple's own traffic.
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Mpeg1Layer3 | Self::Mpeg2Layer3 | Self::Mpeg25Layer3 => "MPEG audio file",
            Self::AacLc | Self::HeAac | Self::HeAacV2 | Self::Usac => "AAC audio file",
            Self::Alac => "Apple Lossless audio file",
            Self::Flac => "FLAC audio file",
            Self::Wav => "WAV audio file",
            Self::Aiff => "AIFF audio file",
        }
    }

    /// The extension of the file on the phone, with its dot.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Mpeg1Layer3 | Self::Mpeg2Layer3 | Self::Mpeg25Layer3 => ".mp3",
            Self::AacLc | Self::HeAac | Self::HeAacV2 | Self::Usac | Self::Alac => ".m4a",
            Self::Flac => ".flac",
            Self::Wav => ".wav",
            Self::Aiff => ".aif",
        }
    }

    /// Every extension a phone file of some supported format has.
    pub const EXTENSIONS: [&'static str; 5] = [".m4a", ".mp3", ".flac", ".wav", ".aif"];

    /// Lossless formats carry a bit depth; lossy ones have none.
    pub const fn lossless(self) -> bool {
        matches!(self, Self::Alac | Self::Flac | Self::Wav | Self::Aiff)
    }

    /// The phone's own limits for this format: sample rate, channels, bit
    /// depth and bit rate, from the `AudioCodecs` it advertises (iPhone17,2,
    /// iOS 27.0.1). A missing maximum sample rate means none on an iPhone.
    pub fn check_phone_limits(
        self,
        sample_rate: u32,
        channels: u8,
        bits_per_sample: u8,
        bit_rate_kbps: u32,
    ) -> Result<()> {
        let label = self.label();
        ensure!(sample_rate > 0, "{label} audio has no sample rate");
        let max_rate = match self {
            Self::Mpeg1Layer3 | Self::Mpeg2Layer3 | Self::Mpeg25Layer3 => Some(48_000),
            Self::AacLc | Self::Usac => Some(96_000),
            Self::HeAac | Self::HeAacV2 => Some(192_000),
            Self::Alac | Self::Flac | Self::Wav | Self::Aiff => None,
        };
        if let Some(max) = max_rate {
            ensure!(
                sample_rate <= max,
                "the phone plays {label} up to {max} Hz; this is {sample_rate} Hz"
            );
        }
        // AAC leaves Multichannel unset, which Apple Devices reads as allowed.
        let max_channels = if matches!(self, Self::AacLc | Self::HeAac | Self::HeAacV2) {
            8
        } else {
            2
        };
        ensure!(
            (1..=max_channels).contains(&channels),
            "the phone plays {label} with {} channels; this has {channels}",
            if max_channels == 2 {
                "one or two".to_owned()
            } else {
                format!("1 to {max_channels}")
            }
        );
        match self {
            Self::Wav | Self::Aiff => ensure!(
                matches!(bits_per_sample, 8 | 16 | 24),
                "the phone plays {label} at 8, 16 or 24 bits; this is {bits_per_sample}-bit"
            ),
            Self::Alac => ensure!(
                matches!(bits_per_sample, 16 | 20 | 24 | 32),
                "ALAC is 16, 20, 24 or 32 bits; this is {bits_per_sample}-bit"
            ),
            Self::Flac => ensure!(
                (4..=32).contains(&bits_per_sample),
                "FLAC is 4 to 32 bits; this is {bits_per_sample}-bit"
            ),
            _ => ensure!(bits_per_sample == 0, "{label} has no bit depth"),
        }
        if matches!(
            self,
            Self::Mpeg1Layer3 | Self::Mpeg2Layer3 | Self::Mpeg25Layer3
        ) {
            ensure!(
                bit_rate_kbps <= 320,
                "the phone plays MP3 up to 320 kb/s; this is {bit_rate_kbps} kb/s"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_names_round_trip_and_never_collide() {
        let mut codes = std::collections::BTreeSet::new();
        for format in ALL {
            assert!(codes.insert(format.code()));
            assert_eq!(Format::from_code(format.code()), Some(format));
            assert_eq!(Format::from_name(format.name()), Some(format));
            assert!(Format::EXTENSIONS.contains(&format.extension()));
        }
        assert_eq!(Format::from_code(0), None);
    }

    #[test]
    fn apple_numbers_match_the_agent() {
        assert_eq!(Format::Mpeg1Layer3.audio_format(), 301);
        assert_eq!(Format::Mpeg25Layer3.audio_format(), 303);
        assert_eq!(Format::AacLc.audio_format(), 502);
        assert_eq!(Format::HeAacV2.audio_format(), 509);
        assert_eq!(Format::Alac.audio_format(), 601);
        assert_eq!(Format::Flac.audio_format(), 602);
        assert_eq!(Format::Aiff.audio_format(), 701);
        assert_eq!(Format::Usac.audio_format(), 1001);
    }

    #[test]
    fn phone_limits() {
        assert!(Format::AacLc.check_phone_limits(96_000, 2, 0, 256).is_ok());
        assert!(
            Format::AacLc
                .check_phone_limits(192_000, 2, 0, 256)
                .is_err()
        );
        assert!(Format::HeAac.check_phone_limits(192_000, 2, 0, 64).is_ok());
        assert!(Format::AacLc.check_phone_limits(48_000, 6, 0, 384).is_ok());
        assert!(Format::Alac.check_phone_limits(192_000, 2, 24, 0).is_ok());
        assert!(Format::Alac.check_phone_limits(44_100, 6, 16, 0).is_err());
        assert!(Format::Wav.check_phone_limits(96_000, 2, 24, 0).is_ok());
        assert!(Format::Wav.check_phone_limits(44_100, 2, 32, 0).is_err());
        assert!(
            Format::Mpeg1Layer3
                .check_phone_limits(44_100, 2, 0, 320)
                .is_ok()
        );
        assert!(
            Format::Mpeg1Layer3
                .check_phone_limits(44_100, 2, 0, 0)
                .is_ok()
        );
        assert!(Format::Flac.check_phone_limits(44_100, 3, 16, 0).is_err());
        assert!(Format::Usac.check_phone_limits(48_000, 2, 0, 0).is_ok());
    }
}
