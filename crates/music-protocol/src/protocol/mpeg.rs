//! Pure artless MPEG wire serialization. Values describe a proposal, never
//! authority. Collision, source, scope, journal and receiver checks stay with
//! callers.
//! [`serialize_format`] sends every other non-ALAC format the same way, with
//! its own `audio_format`, kind and gapless fields.
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use plist::Value;

use crate::{
    catalogue::presentation::Presentation,
    media::{
        format::Format,
        mpeg::projection::{CodedTimingProposal, Metadata},
    },
};

pub use crate::media::mpeg::gapless::Gapless;

pub struct Envelope {
    pub database_pid: u64,
    pub primary_container_pid: u64,
    pub before_anchor: u32,
    pub timestamp: SystemTime,
}
pub struct Ids {
    pub track: u64,
    pub album: u64,
    pub artist: u64,
}
pub enum Timing {
    Omitted,
    Coded {
        samples: u64,
        total_time_ms: u64,
    },
    /// An MPEG Layer III stream as Apple's sync times it: `samples` are the
    /// ones played, `total_time_ms` every counted frame's; the gapless
    /// fields hold the rest.
    Played {
        samples: u64,
        total_time_ms: u64,
    },
}
pub struct TrackFields<'a> {
    pub title: &'a str,
    pub sort_title: &'a str,
    pub artist: &'a str,
    pub sort_artist: &'a str,
    pub album: &'a str,
    pub sort_album: &'a str,
    pub bit_rate_kbps: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub timing: Timing,
}
fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

/// Codec-independent native item fields, kept separate from MPEG timing and
/// artwork. Optional fields are absent unless the source explicitly supplies
/// them. These pure bytes do not qualify a receiver or a collection transition.
#[derive(Default)]
pub struct MetadataFields<'a> {
    pub album_artist: Option<&'a str>,
    pub compilation: Option<bool>,
    pub genre: Option<&'a str>,
    pub composer: Option<(&'a str, &'a str)>,
    pub copyright: Option<&'a str>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    /// The album artist's sort name, when it is not the name.
    pub sort_album_artist: Option<&'a str>,
    /// Sent as `comment`; line breaks and tabs may appear in it.
    pub comment: Option<&'a str>,
}

/// What `avformat_info` and `location` say of the audio beyond its rate,
/// channels and bit rate.
pub struct AvFormat {
    pub format: Format,
    /// PCM bits of a lossless format; 0 for a lossy one.
    pub bits_per_sample: u8,
    /// The bit rate varies from packet to packet; `bit_rate_kbps` is its
    /// average.
    pub variable_bit_rate: bool,
    /// Gapless fields. Apple Devices sends them for ALAC with heuristic 1
    /// and no last-frame resynch, and for an MP3 with the values its rule
    /// finds.
    pub gapless: Option<Gapless>,
}

impl AvFormat {
    /// A constant-bitrate MPEG-1 Layer III stream, as Apple's sync sent one.
    pub const MPEG1_CBR: Self = Self {
        format: Format::Mpeg1Layer3,
        bits_per_sample: 0,
        variable_bit_rate: false,
        gapless: None,
    };

    fn check(&self, fields: &TrackFields<'_>) -> Result<()> {
        let rate = fields.sample_rate;
        let kbps = fields.bit_rate_kbps;
        match self.format {
            Format::Mpeg1Layer3 => ensure!(
                self.bits_per_sample == 0
                    && [32000, 44100, 48000].contains(&rate)
                    && [1, 2].contains(&fields.channels)
                    && if self.variable_bit_rate {
                        (8..=320).contains(&kbps)
                    } else {
                        [
                            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                        ]
                        .contains(&kbps)
                    },
                "invalid MPEG-1 Layer III source format"
            ),
            Format::Alac => anyhow::bail!("ALAC has its own insert serializer"),
            format => {
                ensure!(
                    kbps > 0 && (format.lossless() == (self.bits_per_sample > 0)),
                    "invalid {} source format",
                    format.label()
                );
                format.check_phone_limits(
                    rate,
                    fields.channels,
                    self.bits_per_sample,
                    u32::from(kbps),
                )?;
            }
        }
        Ok(())
    }
}

pub fn serialize(envelope: Envelope, ids: Ids, fields: TrackFields<'_>) -> Result<Vec<u8>> {
    serialize_metadata(envelope, ids, fields, MetadataFields::default())
}

pub fn serialize_metadata(
    envelope: Envelope,
    ids: Ids,
    fields: TrackFields<'_>,
    metadata: MetadataFields<'_>,
) -> Result<Vec<u8>> {
    serialize_format(envelope, ids, fields, metadata, AvFormat::MPEG1_CBR)
}

pub fn serialize_format(
    envelope: Envelope,
    ids: Ids,
    fields: TrackFields<'_>,
    metadata: MetadataFields<'_>,
    audio: AvFormat,
) -> Result<Vec<u8>> {
    ensure!(
        envelope.database_pid != 0 && envelope.primary_container_pid != 0,
        "MPEG catalogue identity missing"
    );
    ensure!(
        ids.track != 0
            && ids.album != 0
            && ids.artist != 0
            && ids.track != ids.album
            && ids.track != ids.artist
            && ids.album != ids.artist,
        "MPEG proposed identities are invalid"
    );
    for text in [
        fields.title,
        fields.sort_title,
        fields.artist,
        fields.sort_artist,
        fields.album,
        fields.sort_album,
    ] {
        ensure!(
            !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
            "invalid MPEG wire text"
        );
    }
    audio.check(&fields)?;
    let revision = envelope
        .before_anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    let mut item = dict([
        ("album_pid", (ids.album as i64).into()),
        ("artist_pid", (ids.artist as i64).into()),
        ("title", fields.title.into()),
        ("sort_name", fields.sort_title.into()),
        ("artist", fields.artist.into()),
        ("sort_artist", fields.sort_artist.into()),
        ("album", fields.album.into()),
        ("sort_album", fields.sort_album.into()),
        ("is_song", true.into()),
        ("remember_bookmark", false.into()),
        ("artwork_checksum", 0u64.into()),
    ]);
    let mut av = dict([
        (
            "audio_format",
            u64::from(audio.format.audio_format()).into(),
        ),
        ("bit_rate", u64::from(fields.bit_rate_kbps).into()),
        ("sample_rate", Value::Real(f64::from(fields.sample_rate))),
        ("channels", u64::from(fields.channels).into()),
    ]);
    let timing = match fields.timing {
        Timing::Omitted => None,
        Timing::Coded {
            samples,
            total_time_ms,
        } => {
            ensure!(
                samples
                    .checked_mul(1000)
                    .and_then(|s| s.checked_div(u64::from(fields.sample_rate)))
                    == Some(total_time_ms),
                "MPEG coded duration and milliseconds disagree"
            );
            Some((samples, total_time_ms))
        }
        Timing::Played {
            samples,
            total_time_ms,
        } => {
            ensure!(
                matches!(
                    audio.format,
                    Format::Mpeg1Layer3 | Format::Mpeg2Layer3 | Format::Mpeg25Layer3
                ) && audio.gapless.is_some(),
                "only an MPEG Layer III stream with gapless fields sends its played samples"
            );
            Some((samples, total_time_ms))
        }
    };
    if let Some((samples, total_time_ms)) = timing {
        ensure!(samples > 0 && total_time_ms > 0, "empty MPEG duration");
        item.as_dictionary_mut()
            .unwrap()
            .insert("total_time_ms".into(), total_time_ms.into());
        av.as_dictionary_mut()
            .unwrap()
            .insert("duration".into(), samples.into());
    }
    if let Some(gapless) = &audio.gapless {
        let av = av.as_dictionary_mut().unwrap();
        for (name, value) in [
            ("gapless_encoding_delay", gapless.delay),
            ("gapless_encoding_drain", gapless.drain),
            ("gapless_heuristic_info", u64::from(gapless.heuristic)),
            ("gapless_last_frame_resynch", gapless.resynch),
        ] {
            av.insert(name.into(), value.into());
        }
    }
    let item_fields = item.as_dictionary_mut().unwrap();
    for (name, value) in [
        ("album_artist", metadata.album_artist),
        (
            "sort_album_artist",
            metadata
                .album_artist
                .map(|name| metadata.sort_album_artist.unwrap_or(name)),
        ),
        ("genre", metadata.genre),
        ("composer", metadata.composer.map(|c| c.0)),
        ("sort_composer", metadata.composer.map(|c| c.1)),
        ("copyright", metadata.copyright),
    ] {
        if let Some(value) = value {
            ensure!(
                !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
                "invalid MPEG item metadata"
            );
            item_fields.insert(name.into(), value.into());
        }
    }
    ensure!(
        metadata.compilation != Some(true) || metadata.album_artist.is_some(),
        "compilation requires an explicit album artist"
    );
    if metadata.compilation == Some(true) {
        item_fields.insert("is_compilation".into(), true.into());
    }
    if let Some(comment) = metadata.comment {
        ensure!(
            !comment.is_empty()
                && comment.len() <= 4096
                && !comment
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
            "invalid MPEG comment"
        );
        item_fields.insert("comment".into(), comment.into());
    }
    for (number, total) in [
        (metadata.track, metadata.track_total),
        (metadata.disc, metadata.disc_total),
    ] {
        ensure!(
            number != Some(0)
                && total != Some(0)
                && (total.is_none() || number.is_some())
                && !matches!((number,total),(Some(n),Some(t)) if n>t),
            "invalid MPEG numbering"
        );
    }
    for (name, value) in [
        ("year", metadata.year),
        ("track_number", metadata.track),
        ("track_count", metadata.track_total),
        ("disc_number", metadata.disc),
        ("disc_count", metadata.disc_total),
    ] {
        if let Some(value) = value {
            ensure!(value > 0, "zero MPEG numeric metadata");
            item_fields.insert(name.into(), u64::from(value).into());
        }
    }
    let own = dict([
        ("operation", "insert_track".into()),
        ("pid", (ids.track as i64).into()),
        ("item", item),
        ("avformat_info", av),
        ("location", dict([("kind", audio.format.kind().into())])),
        (
            "item_stats",
            dict([
                ("play_count_recent", 0u64.into()),
                ("play_count_user", 0u64.into()),
                ("skip_count_recent", 0u64.into()),
                ("skip_count_user", 0u64.into()),
                ("has_been_played", false.into()),
            ]),
        ),
    ]);
    let db = dict([
        ("operation", "update_db_info".into()),
        ("pid", (envelope.database_pid as i64).into()),
        (
            "db_info",
            dict([(
                "primary_container_pid",
                (envelope.primary_container_pid as i64).into(),
            )]),
        ),
    ]);
    let envelope = dict([
        ("revision", u64::from(revision).into()),
        ("timestamp", Value::Date(envelope.timestamp.into())),
        ("operations", Value::Array(vec![db, own])),
    ]);
    let mut result = Vec::new();
    envelope.to_writer_binary(&mut result)?;
    ensure!(result.len() <= 64 * 1024, "MPEG operation too large");
    Ok(result)
}

/// The insert operation of a source's projection and coded timing, once the
/// application has accepted the source. `presentation` is what the insert
/// sends besides the names (the catalogue's sort names and the comment);
/// `existing_composer` is the label and stored sort label of the composer the
/// catalogue already holds under the source's name, which an insert sends in
/// place of the source's spelling. A composer the catalogue lacks goes with
/// the presentation's sort name, else its own name.
pub fn serialize_source(
    envelope: Envelope,
    ids: Ids,
    metadata: &Metadata,
    timing: &CodedTimingProposal,
    presentation: &Presentation<'_>,
    existing_composer: Option<(&str, &str)>,
) -> Result<Vec<u8>> {
    let composer = match (existing_composer, metadata.composer.as_deref()) {
        (Some(pair), _) => Some(pair),
        (None, Some(name)) => Some((name, presentation.sort_composer.unwrap_or(name))),
        (None, None) => None,
    };
    serialize_format(
        envelope,
        ids,
        TrackFields {
            title: &metadata.title,
            sort_title: presentation.sort_title,
            artist: &metadata.artist,
            sort_artist: presentation.sort_artist,
            album: &metadata.album,
            sort_album: presentation.sort_album,
            bit_rate_kbps: timing.bit_rate_kbps,
            sample_rate: timing.sample_rate,
            channels: timing.channels,
            timing: match timing.format {
                Format::Mpeg1Layer3 | Format::Mpeg2Layer3 | Format::Mpeg25Layer3 => {
                    Timing::Played {
                        samples: timing.coded_samples,
                        total_time_ms: timing.total_time_ms_floor,
                    }
                }
                _ => Timing::Coded {
                    samples: timing.coded_samples,
                    total_time_ms: timing.total_time_ms_floor,
                },
            },
        },
        MetadataFields {
            album_artist: metadata.album_artist.as_deref(),
            compilation: metadata.compilation,
            genre: metadata.genre.as_deref(),
            composer,
            copyright: metadata.copyright.as_deref(),
            year: metadata.year,
            track: metadata.track,
            track_total: metadata.track_total,
            disc: metadata.disc,
            disc_total: metadata.disc_total,
            sort_album_artist: presentation.sort_album_artist,
            comment: presentation.comment,
        },
        AvFormat {
            format: timing.format,
            bits_per_sample: timing.bits_per_sample,
            variable_bit_rate: timing.variable_bit_rate,
            gapless: timing.gapless.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        time::{Duration, UNIX_EPOCH},
    };

    fn envelope() -> Envelope {
        Envelope {
            database_pid: 11,
            primary_container_pid: 12,
            before_anchor: 40,
            timestamp: UNIX_EPOCH + Duration::from_secs(7),
        }
    }
    fn ids() -> Ids {
        Ids {
            track: 1001,
            album: 5000,
            artist: 6000,
        }
    }
    fn fields(bit_rate_kbps: u16, sample_rate: u32, samples: u64) -> TrackFields<'static> {
        TrackFields {
            title: "Title",
            sort_title: "Title",
            artist: "Artist",
            sort_artist: "Artist",
            album: "Album",
            sort_album: "Album",
            bit_rate_kbps,
            sample_rate,
            channels: 2,
            timing: Timing::Coded {
                samples,
                total_time_ms: samples * 1000 / u64::from(sample_rate),
            },
        }
    }
    fn insert(bytes: &[u8]) -> plist::Dictionary {
        let root = Value::from_reader(Cursor::new(bytes)).unwrap();
        root.as_dictionary().unwrap()["operations"]
            .as_array()
            .unwrap()[1]
            .as_dictionary()
            .unwrap()
            .clone()
    }

    #[test]
    fn each_format_names_its_audio_format_kind_and_gapless_fields() {
        let flac = serialize_format(
            envelope(),
            ids(),
            fields(900, 44100, 441_000),
            MetadataFields::default(),
            AvFormat {
                format: Format::Flac,
                bits_per_sample: 16,
                variable_bit_rate: true,
                gapless: Some(Gapless::NONE),
            },
        )
        .unwrap();
        let op = insert(&flac);
        let av = op["avformat_info"].as_dictionary().unwrap();
        assert_eq!(av["audio_format"].as_unsigned_integer(), Some(602));
        assert_eq!(av["duration"].as_unsigned_integer(), Some(441_000));
        assert_eq!(av["gapless_heuristic_info"].as_unsigned_integer(), Some(1));
        assert_eq!(av["gapless_encoding_drain"].as_unsigned_integer(), Some(0));
        assert_eq!(
            op["location"].as_dictionary().unwrap()["kind"].as_string(),
            Some("FLAC audio file")
        );
        // An MPEG-1 insert sends no gapless fields and keeps its kind.
        let mp3 = serialize(envelope(), ids(), fields(128, 44100, 44100 * 10)).unwrap();
        let op = insert(&mp3);
        let av = op["avformat_info"].as_dictionary().unwrap();
        assert_eq!(av["audio_format"].as_unsigned_integer(), Some(301));
        assert!(!av.contains_key("gapless_heuristic_info"));
        assert_eq!(
            op["location"].as_dictionary().unwrap()["kind"].as_string(),
            Some("MPEG audio file")
        );
    }

    #[test]
    fn an_mp3_sends_apples_played_samples_and_gapless_fields() {
        // The reference MP3, as Apple's sync sent it.
        let mut track = fields(128, 44100, 0);
        track.timing = Timing::Played {
            samples: 11_162_592,
            total_time_ms: 253_152,
        };
        let audio = |gapless| AvFormat {
            gapless,
            ..AvFormat::MPEG1_CBR
        };
        let reference = Gapless {
            delay: 528,
            drain: 912,
            heuristic: 0x0200_0003,
            resynch: 4_047_097,
        };
        let bytes = serialize_format(
            envelope(),
            ids(),
            track,
            MetadataFields::default(),
            audio(Some(reference)),
        )
        .unwrap();
        let op = insert(&bytes);
        assert_eq!(
            op["item"].as_dictionary().unwrap()["total_time_ms"].as_unsigned_integer(),
            Some(253_152)
        );
        let av = op["avformat_info"].as_dictionary().unwrap();
        for (key, value) in [
            ("duration", 11_162_592),
            ("gapless_encoding_delay", 528),
            ("gapless_encoding_drain", 912),
            ("gapless_heuristic_info", 33_554_435),
            ("gapless_last_frame_resynch", 4_047_097),
        ] {
            assert_eq!(av[key].as_unsigned_integer(), Some(value), "{key}");
        }
        // Played samples need the gapless fields, and an MPEG stream.
        let played = || {
            let mut track = fields(128, 44100, 0);
            track.timing = Timing::Played {
                samples: 1000,
                total_time_ms: 52,
            };
            track
        };
        assert!(
            serialize_format(
                envelope(),
                ids(),
                played(),
                MetadataFields::default(),
                audio(None)
            )
            .is_err()
        );
        let flac = AvFormat {
            format: Format::Flac,
            bits_per_sample: 16,
            variable_bit_rate: true,
            gapless: Some(Gapless::NONE),
        };
        let mut track = played();
        track.bit_rate_kbps = 900;
        assert!(
            serialize_format(envelope(), ids(), track, MetadataFields::default(), flac).is_err()
        );
    }

    #[test]
    fn formats_outside_the_phone_limits_refuse() {
        let av = |format, bits_per_sample, variable_bit_rate| AvFormat {
            format,
            bits_per_sample,
            variable_bit_rate,
            gapless: None,
        };
        let send = |kbps, rate, audio| {
            serialize_format(
                envelope(),
                ids(),
                fields(kbps, rate, 88200),
                MetadataFields::default(),
                audio,
            )
        };
        // A VBR MP3's average need not be a table rate; a CBR one's must.
        assert!(send(187, 44100, av(Format::Mpeg1Layer3, 0, true)).is_ok());
        assert!(send(187, 44100, av(Format::Mpeg1Layer3, 0, false)).is_err());
        assert!(send(352, 44100, av(Format::Mpeg1Layer3, 0, true)).is_err());
        assert!(send(1411, 44100, av(Format::Wav, 16, false)).is_ok());
        assert!(send(1411, 44100, av(Format::Wav, 0, false)).is_err());
        assert!(send(1411, 44100, av(Format::Alac, 16, false)).is_err());
        assert!(send(256, 192_000, av(Format::AacLc, 0, false)).is_err());
    }
}
