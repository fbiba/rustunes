//! The tags and coded timing an insert sends for a non-ALAC source: what a
//! source's inspection projects into the phone's catalogue fields, and the
//! duration, bit rate and gapless fields Apple's rule gives it. This is
//! mechanism only: whether a source may be inserted at all, and how a plan
//! names what it cannot send, stay with the application.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    Source, Tags,
    gapless::Gapless,
    timing::{BitrateMode, SeekHeader},
};
use crate::media::{
    audio_source::Stream,
    format::Format,
    metadata::{ScalarMetadata, tag_values},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Metadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub copyright: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Explicit sort tags; see `ScalarMetadata`'s sort names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_composer: Option<String>,
}

impl Metadata {
    /// The tags an insert of this projection sends, as the row it makes must
    /// say them.
    pub fn scalar(&self) -> ScalarMetadata<'_> {
        ScalarMetadata {
            title: &self.title,
            artist: &self.artist,
            album: &self.album,
            album_artist: self.album_artist.as_deref(),
            genre: self.genre.as_deref(),
            composer: self.composer.as_deref(),
            copyright: self.copyright.as_deref(),
            year: self.year,
            content_rating: None,
            compilation: self.compilation,
            track: self.track,
            track_total: self.track_total,
            disc: self.disc,
            disc_total: self.disc_total,
            comment: self.comment.as_deref(),
            sort_title: self.sort_title.as_deref(),
            sort_artist: self.sort_artist.as_deref(),
            sort_album: self.sort_album.as_deref(),
            sort_album_artist: self.sort_album_artist.as_deref(),
            sort_composer: self.sort_composer.as_deref(),
        }
    }
}

/// No bit depth, ALAC configuration or artwork checksum is synthesized from
/// MPEG headers. An MPEG Layer III proposal carries the timing and gapless
/// fields Apple's rule gives; another format's names it, and carries the bit
/// depth and encoder trim its container declares. `coded_samples` is the
/// duration sent: the samples played.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CodedTimingProposal {
    pub basis: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_rate_kbps: u16,
    pub coded_samples: u64,
    pub total_time_ms_floor: u64,
    pub native_mapping_qualified: bool,
    #[serde(skip_serializing_if = "is_mpeg1")]
    pub format: Format,
    #[serde(skip_serializing_if = "is_zero")]
    pub bits_per_sample: u8,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub variable_bit_rate: bool,
    /// Encoder delay and padding the insert sends as gapless fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gapless: Option<Gapless>,
}

fn is_mpeg1(format: &Format) -> bool {
    *format == Format::Mpeg1Layer3
}

fn is_zero(bits: &u8) -> bool {
    *bits == 0
}

/// Tag fields a projection reads, by their id in each container: ID3 frames,
/// MP4 atoms (escaped as the reader reports them), Vorbis comment keys, RIFF
/// INFO ids and AIFF chunks. Encoder names and pictures say nothing a catalogue
/// holds.
const PROJECTED_FIELDS: [&str; 69] = [
    "TIT2",
    "TPE1",
    "TALB",
    "TPE2",
    "TCON",
    "TCOM",
    "TCOP",
    "TDRC",
    "TYER",
    "TRCK",
    "TPOS",
    "TCMP",
    "APIC",
    "TSSE",
    r"\xa9nam",
    r"\xa9ART",
    r"\xa9alb",
    "aART",
    r"\xa9gen",
    "gnre",
    r"\xa9wrt",
    "cprt",
    r"\xa9day",
    "trkn",
    "disk",
    "cpil",
    "covr",
    r"\xa9too",
    "TITLE",
    "ARTIST",
    "ALBUM",
    "ALBUMARTIST",
    "GENRE",
    "COMPOSER",
    "COPYRIGHT",
    "DATE",
    "TRACKNUMBER",
    "TRACKTOTAL",
    "DISCNUMBER",
    "DISCTOTAL",
    "COMPILATION",
    "INAM",
    "IART",
    "IPRD",
    "IGNR",
    "ICOP",
    "ICRD",
    "ITRK",
    "IPRT",
    "ISFT",
    "ENCODER",
    "NAME",
    "AUTH",
    "(c) ",
    "TSOT",
    "TSOP",
    "TSOA",
    "TSO2",
    "TSOC",
    "sonm",
    "soar",
    "soal",
    "soaa",
    "soco",
    "TITLESORT",
    "ARTISTSORT",
    "ALBUMSORT",
    "ALBUMARTISTSORT",
    "COMPOSERSORT",
];

/// Tag fields that hold a comment; see `Tags::catalogue_comments`.
const COMMENT_FIELDS: [&str; 6] = ["COMM", r"\xa9cmt", "COMMENT", "DESCRIPTION", "ICMT", "ANNO"];

/// The tag fields of `source` that a projection does not read, by id: they stay
/// in the file and do not reach the phone's catalogue. A comment without a
/// description is sent; an empty comment frame (no description, no text) has
/// nothing to send, and Apple's own sync sent no comment for the reference
/// MP3's empty frame. A described one (iTunNORM and the like) is not a
/// catalogue comment.
pub fn unprojected_frames(source: &Source) -> BTreeSet<&str> {
    let comments_projected = source
        .metadata
        .comments
        .iter()
        .all(|c| c.description.is_empty());
    source
        .frames
        .iter()
        .filter(|f| {
            !(COMMENT_FIELDS.contains(&f.id.as_str()) && comments_projected)
                && !PROJECTED_FIELDS.contains(&f.id.as_str())
        })
        .map(|f| f.id.as_str())
        .collect()
}

/// A field's one value, as Apple's readers take one: a tag that holds the
/// field more than once has to be edited to keep one. An empty value counts
/// for none (`metadata::tag_value`).
fn text(values: &[String], field: &str, required: bool) -> Result<Option<String>> {
    let values: Vec<_> = tag_values(values).collect();
    ensure!(
        values.len() <= 1,
        "{field} has {} values in the file's tag; edit the tag to keep one",
        values.len()
    );
    let Some(value) = values.first() else {
        ensure!(!required, "missing {field}");
        return Ok(None);
    };
    valid(value, field)?;
    Ok(Some((*value).to_owned()))
}

/// A value that says something, held to the catalogue's text rules.
fn valid(value: &str, field: &str) -> Result<()> {
    ensure!(
        value.len() <= 4096 && !value.chars().any(char::is_control),
        "invalid {field} text"
    );
    Ok(())
}

/// A FLAC's Vorbis comments name one artist per `ARTIST` entry and one
/// composer per `COMPOSER` entry. Apple's sync reads no FLAC, so this is the
/// host's rule: the distinct entries, each valid on its own, joined in file
/// order with ", " into the one value the field holds. A single entry stays as
/// it is, and an empty entry is no entry.
fn joined(values: &[String], field: &str) -> Result<Vec<String>> {
    let mut distinct: Vec<&str> = Vec::new();
    for value in tag_values(values) {
        if !distinct.contains(&value) {
            distinct.push(value);
        }
    }
    if distinct.len() > 1 {
        for value in &distinct {
            valid(value, field)?;
        }
        return Ok(vec![distinct.join(", ")]);
    }
    Ok(distinct.into_iter().map(str::to_owned).collect())
}

fn numbering(number: Option<u32>, total: Option<u32>, field: &str) -> Result<()> {
    ensure!(
        number != Some(0) && total != Some(0),
        "zero {field} numbering"
    );
    ensure!(
        total.is_none() || number.is_some(),
        "{field} total without a number"
    );
    if let (Some(number), Some(total)) = (number, total) {
        ensure!(number <= total, "{field} number exceeds total");
    }
    Ok(())
}

/// The catalogue fields `source`'s tags give. A FLAC's repeated artists,
/// composers and their sort names are joined (see `joined`); every other
/// repeated field, and every repeated field of another format, is refused.
/// An empty value is no value.
pub fn project(source: &Source) -> Result<Metadata> {
    project_tags(&source.metadata, source.format() == Format::Flac)
}

fn project_tags(tags: &Tags, flac: bool) -> Result<Metadata> {
    numbering(tags.track, tags.track_total, "track")?;
    numbering(tags.disc, tags.disc_total, "disc")?;
    // A full date (`2024-05-01`, `2024-05-01T07:00:00Z`) gives its year.
    let year = text(&tags.release_date, "release date", false)?
        .map(|value| {
            ensure!(
                value.len() >= 4
                    && value.as_bytes()[..4].iter().all(u8::is_ascii_digit)
                    && (value.len() == 4 || value.as_bytes()[4] == b'-'),
                "a release date must start with a four-digit year"
            );
            let year = value[..4].parse::<u32>()?;
            ensure!(year != 0, "zero release year");
            Ok(year)
        })
        .transpose()?;
    let genre = text(&tags.genre, "genre", false)?;
    if let Some(genre) = &genre {
        ensure!(
            !genre.bytes().all(|b| b.is_ascii_digit()) && !genre.starts_with('('),
            "numeric ID3 genres require an explicit label mapping"
        );
    }
    let (artist, sort_artist, composer, sort_composer) = if flac {
        (
            joined(&tags.artist, "artist")?,
            joined(&tags.sort_artist, "sort artist")?,
            joined(&tags.composer, "composer")?,
            joined(&tags.sort_composer, "sort composer")?,
        )
    } else {
        (
            tags.artist.clone(),
            tags.sort_artist.clone(),
            tags.composer.clone(),
            tags.sort_composer.clone(),
        )
    };
    Ok(Metadata {
        comment: crate::media::metadata::comment_text(&tags.catalogue_comments())?
            .map(str::to_owned),
        sort_title: text(&tags.sort_title, "sort title", false)?,
        sort_artist: text(&sort_artist, "sort artist", false)?,
        sort_album: text(&tags.sort_album, "sort album", false)?,
        sort_album_artist: text(&tags.sort_album_artist, "sort album artist", false)?,
        sort_composer: text(&sort_composer, "sort composer", false)?,
        title: text(&tags.title, "title", true)?.context("missing title")?,
        artist: text(&artist, "artist", true)?.context("missing artist")?,
        album: text(&tags.album, "album", true)?.context("missing album")?,
        album_artist: text(&tags.album_artist, "album artist", false)?,
        genre,
        composer: text(&composer, "composer", false)?,
        copyright: text(&tags.copyright, "copyright", false)?,
        year,
        track: tags.track,
        track_total: tags.track_total,
        disc: tags.disc,
        disc_total: tags.disc_total,
        compilation: tags.compilation,
    })
}

pub fn coded_timing(source: &Source) -> Result<CodedTimingProposal> {
    let audio = &source.fingerprint.audio;
    // Checking these joins matters because inspection reports are serializable
    // facts, not a start capability. Never consume a caller's rounded duration.
    source.transferred_identity()?;
    if let Some(stream) = &source.stream {
        return stream_timing(source, stream);
    }
    let timing = source.frame_timing()?;
    let frames = &timing.physical;
    ensure!(
        timing.frame_bytes_sha256 == audio.frame_bytes_sha256
            && frames.frame_count == audio.frame_count
            && frames.coded_duration.samples == audio.coded_samples
            && frames.coded_duration.sample_rate == audio.sample_rate,
        "source timing and encoded frame identity disagree"
    );
    // An Info header marks a constant-bitrate stream, a Xing or VBRI header a
    // variable one; Apple's rule reads them for the frame count, bit rate and
    // encoder trim.
    ensure!(
        match &timing.header {
            None | Some(SeekHeader::Vbri(_)) => timing.excluding_xing_info_frame.is_none(),
            Some(SeekHeader::Info(_) | SeekHeader::Xing(_)) => {
                timing.excluding_xing_info_frame.is_some()
            }
        },
        "seek header and frame counts disagree"
    );
    ensure!(
        frames.bitrates.iter().map(|b| b.frames).sum::<u32>() == audio.frame_count,
        "frame bit rates do not cover the stream"
    );
    // Apple's bit rate is the average under a Xing or VBRI header.
    let constant = frames.bitrate_mode == BitrateMode::Constant
        && frames.bitrates.len() == 1
        && frames.bitrates[0].frames == audio.frame_count
        && !matches!(
            timing.header,
            Some(SeekHeader::Xing(_) | SeekHeader::Vbri(_))
        );
    apple_timing(source, Format::Mpeg1Layer3, !constant)
}

/// An MPEG Layer III stream's fields as Apple's sync sends them: the display
/// time of the frames it counts, the samples played, its bit rate and the
/// encoder trim it finds.
fn apple_timing(source: &Source, format: Format, variable: bool) -> Result<CodedTimingProposal> {
    let audio = &source.fingerprint.audio;
    let apple = source
        .apple_timing
        .as_ref()
        .context("Apple's MPEG timing unavailable")?;
    ensure!(
        apple.duration > 0 && apple.total_time_ms > 0,
        "empty coded duration"
    );
    Ok(CodedTimingProposal {
        basis: "apple_counted_frames_and_played_samples",
        sample_rate: audio.sample_rate,
        channels: audio.channels,
        bit_rate_kbps: u16::try_from(apple.bit_rate_kbps).context("bit rate out of range")?,
        coded_samples: apple.duration,
        total_time_ms_floor: apple.total_time_ms,
        native_mapping_qualified: false,
        format,
        bits_per_sample: 0,
        variable_bit_rate: variable,
        gapless: Some(Gapless {
            delay: apple.delay.into(),
            drain: apple.drain.into(),
            heuristic: apple.heuristic,
            resynch: apple.resynch,
        }),
    })
}

/// Another format's duration and gapless fields. MPEG-2 and 2.5 Layer III go
/// as MPEG-1 does, by Apple's rule. AAC sends the samples its edit list
/// keeps, with the trimmed ones as gapless delay and drain. PCM and FLAC send
/// every sample, with gapless fields as Apple Devices sends them for ALAC
/// (heuristic 1, no trim).
fn stream_timing(source: &Source, stream: &Stream) -> Result<CodedTimingProposal> {
    let audio = &source.fingerprint.audio;
    let encoded = &stream.encoded;
    ensure!(
        encoded.sample_rate == audio.sample_rate
            && encoded.channels == audio.channels
            && encoded.coded_samples == audio.coded_samples
            && encoded.audio_bytes == audio.audio_bytes,
        "source stream and encoded identity disagree"
    );
    stream.format.check_phone_limits(
        audio.sample_rate,
        audio.channels,
        stream.bits_per_sample,
        stream.bit_rate_kbps,
    )?;
    let (basis, samples, gapless) = match stream.format {
        Format::Mpeg2Layer3 | Format::Mpeg25Layer3 => {
            return apple_timing(source, stream.format, stream.variable_bit_rate);
        }
        Format::AacLc | Format::HeAac | Format::HeAacV2 | Format::Usac => (
            "edit_list_samples_with_declared_encoder_trim",
            stream.valid_samples,
            Some(Gapless {
                delay: stream.encoder_delay,
                drain: stream.encoder_padding,
                ..Gapless::NONE
            }),
        ),
        Format::Flac | Format::Wav | Format::Aiff => {
            ("pcm_frames", audio.coded_samples, Some(Gapless::NONE))
        }
        Format::Mpeg1Layer3 | Format::Alac => {
            anyhow::bail!("{} has its own reader", stream.format.label())
        }
    };
    Ok(CodedTimingProposal {
        basis,
        sample_rate: audio.sample_rate,
        channels: audio.channels,
        bit_rate_kbps: u16::try_from(stream.bit_rate_kbps).context("bit rate out of range")?,
        coded_samples: samples,
        total_time_ms_floor: milliseconds(samples, audio.sample_rate)?,
        native_mapping_qualified: false,
        format: stream.format,
        bits_per_sample: stream.bits_per_sample,
        variable_bit_rate: stream.variable_bit_rate,
        gapless,
    })
}

fn milliseconds(samples: u64, sample_rate: u32) -> Result<u64> {
    let milliseconds = samples
        .checked_mul(1000)
        .and_then(|n| n.checked_div(u64::from(sample_rate)))
        .context("coded duration overflow or zero sample rate")?;
    ensure!(milliseconds > 0, "empty coded duration");
    Ok(milliseconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tags(composer: &[&str], sort_composer: &[&str]) -> Tags {
        Tags {
            title: vec!["Title".into()],
            artist: vec!["Artist A".into()],
            album: vec!["Album".into()],
            composer: composer.iter().map(|c| c.to_string()).collect(),
            sort_composer: sort_composer.iter().map(|c| c.to_string()).collect(),
            ..Tags::default()
        }
    }

    /// The composer and sort composer `composer` and `sort` project to.
    fn composers(
        composer: &[&str],
        sort: &[&str],
        flac: bool,
    ) -> Result<(Option<String>, Option<String>)> {
        let m = project_tags(&tags(composer, sort), flac)?;
        Ok((m.composer, m.sort_composer))
    }

    #[test]
    fn a_flac_joins_its_distinct_composers_in_file_order() {
        let joined = |c: &[&str], s: &[&str]| composers(c, s, true).unwrap();
        assert_eq!(
            joined(&["Artist B", "Artist C"], &[]),
            (Some("Artist B, Artist C".into()), None)
        );
        assert_eq!(
            joined(&["Artist C", "Artist B", "Artist C"], &[])
                .0
                .as_deref(),
            Some("Artist C, Artist B")
        );
        assert_eq!(
            joined(&["Artist B", "Artist B"], &[]).0.as_deref(),
            Some("Artist B")
        );
        // A single entry stays as it is, a separator in it too.
        assert_eq!(joined(&["Artist B"], &[]).0.as_deref(), Some("Artist B"));
        assert_eq!(
            joined(&["Artist B;Artist C"], &[]).0.as_deref(),
            Some("Artist B;Artist C")
        );
        // Sort composers join the same way; an empty one says nothing.
        assert_eq!(
            joined(&["Artist B", "Artist C"], &["B, Artist", "C, Artist"]),
            (
                Some("Artist B, Artist C".into()),
                Some("B, Artist, C, Artist".into())
            )
        );
        assert_eq!(joined(&["Artist B"], &["", ""]).1, None);
        assert_eq!(joined(&[], &[]), (None, None));
    }

    #[test]
    fn each_joined_entry_and_the_join_are_checked_as_one_value_is() {
        let refused = |c: &[&str]| composers(c, &[], true).unwrap_err().to_string();
        assert_eq!(refused(&["Artist B", "Artist\nC"]), "invalid composer text");
        // The join's length is held to a single value's.
        let (a, b) = ("a".repeat(2047), "b".repeat(2047));
        let fits = composers(&[&a, &b], &[], true).unwrap().0.unwrap();
        assert_eq!(fits.len(), 4096);
        assert_eq!(refused(&[&a, &format!("{b}b")]), "invalid composer text");
        assert_eq!(refused(&[&"a".repeat(4097)]), "invalid composer text");
        let sort = composers(&["Artist B"], &["B", "C\t"], true).unwrap_err();
        assert_eq!(sort.to_string(), "invalid sort composer text");
    }

    #[test]
    fn an_empty_value_is_no_value() {
        let composer = |c: &[&str], flac: bool| composers(c, &[], flac).unwrap().0;
        // An empty entry drops out of a FLAC's join, and a lone one leaves the field absent.
        assert_eq!(
            composer(&["Artist B", ""], true).as_deref(),
            Some("Artist B")
        );
        assert_eq!(
            composer(&["", "Artist B", ""], true).as_deref(),
            Some("Artist B")
        );
        assert_eq!(composer(&[""], true), None);
        assert_eq!(composer(&["", ""], true), None);
        // The same in a format that keeps one value per field.
        assert_eq!(composer(&[""], false), None);
        assert_eq!(
            composer(&["Artist B", ""], false).as_deref(),
            Some("Artist B")
        );
        // Two values that say something are still two.
        let two = composers(&["Artist B", "", "Artist C"], &[], false).unwrap_err();
        assert_eq!(
            two.to_string(),
            "composer has 2 values in the file's tag; edit the tag to keep one"
        );
        // A required field left with no value is missing, and an empty sort
        // name says nothing.
        let mut empty = artists(&[""], &[""]);
        assert_eq!(
            project_tags(&empty, true).unwrap_err().to_string(),
            "missing artist"
        );
        empty.artist = vec!["Artist A".into()];
        empty.genre = vec![String::new()];
        empty.copyright = vec![String::new()];
        empty.album_artist = vec![String::new()];
        empty.release_date = vec![String::new()];
        let m = project_tags(&empty, false).unwrap();
        assert_eq!(
            (
                &m.genre,
                &m.copyright,
                &m.album_artist,
                m.year,
                &m.sort_artist
            ),
            (&None, &None, &None, None, &None)
        );
        // An emptied tag projects as a removed one.
        assert_eq!(
            m,
            project_tags(&artists(&["Artist A"], &[]), false).unwrap()
        );
        assert_eq!(
            project_tags(&artists(&["Artist A"], &[]), true).unwrap(),
            project_tags(&artists(&["Artist A"], &[""]), true).unwrap()
        );
    }

    /// `tags` with `artist` and `sort` as its artists and sort artists.
    fn artists(artist: &[&str], sort: &[&str]) -> Tags {
        Tags {
            artist: artist.iter().map(|a| a.to_string()).collect(),
            sort_artist: sort.iter().map(|a| a.to_string()).collect(),
            ..tags(&[], &[])
        }
    }

    #[test]
    fn a_flac_joins_its_distinct_artists_as_its_composers() {
        let joined = |a: &[&str], s: &[&str]| {
            let m = project_tags(&artists(a, s), true).unwrap();
            (m.artist, m.sort_artist)
        };
        assert_eq!(
            joined(&["Artist A", "Artist B", "Artist C"], &[]),
            ("Artist A, Artist B, Artist C".into(), None)
        );
        assert_eq!(
            joined(
                &["Artist A", "Artist B", "Artist A"],
                &["A, Artist", "B, Artist"]
            ),
            (
                "Artist A, Artist B".into(),
                Some("A, Artist, B, Artist".into())
            )
        );
        assert_eq!(joined(&["Artist A;Artist B"], &[]).0, "Artist A;Artist B");
        assert_eq!(joined(&["Artist A", "Artist B"], &["", ""]).1, None);
        let refused =
            |a: &[&str], s: &[&str]| project_tags(&artists(a, s), true).unwrap_err().to_string();
        assert_eq!(
            joined(&["Artist A", ""], &[]).0,
            "Artist A",
            "an empty artist entry is none"
        );
        assert_eq!(
            refused(&["Artist A"], &["A", "B\t"]),
            "invalid sort artist text"
        );
        assert_eq!(refused(&[], &[]), "missing artist");
    }

    #[test]
    fn other_formats_and_other_fields_keep_one_value() {
        let refused = |c: &[&str], s: &[&str]| composers(c, s, false).unwrap_err().to_string();
        assert_eq!(
            refused(&["Artist B", "Artist C"], &[]),
            "composer has 2 values in the file's tag; edit the tag to keep one"
        );
        assert_eq!(
            refused(&["Artist B"], &["B", "C", "D"]),
            "sort composer has 3 values in the file's tag; edit the tag to keep one"
        );
        let refused =
            |a: &[&str], s: &[&str]| project_tags(&artists(a, s), false).unwrap_err().to_string();
        assert_eq!(
            refused(&["Artist A", "Artist B"], &[]),
            "artist has 2 values in the file's tag; edit the tag to keep one"
        );
        assert_eq!(
            refused(&["Artist A"], &["A", "B"]),
            "sort artist has 2 values in the file's tag; edit the tag to keep one"
        );
        for flac in [true, false] {
            let mut repeated = tags(&["Artist B", "Artist C"], &[]);
            repeated.title.push("Other Title".into());
            assert_eq!(
                project_tags(&repeated, flac).unwrap_err().to_string(),
                "title has 2 values in the file's tag; edit the tag to keep one"
            );
            let mut repeated = tags(&[], &[]);
            repeated.album_artist = vec!["Artist A".into(), "Artist B".into()];
            assert_eq!(
                project_tags(&repeated, flac).unwrap_err().to_string(),
                "album artist has 2 values in the file's tag; edit the tag to keep one"
            );
        }
    }

    /// A FLAC whose Vorbis comment holds `entries`, read as any source is.
    fn flac(entries: &[&str]) -> Source {
        let mut comment = Vec::new();
        comment.extend(4u32.to_le_bytes());
        comment.extend(b"test");
        comment.extend((entries.len() as u32).to_le_bytes());
        for entry in entries {
            comment.extend((entry.len() as u32).to_le_bytes());
            comment.extend(entry.as_bytes());
        }
        let bytes = crate::media::container::flac::tests::flac(&comment, 0);
        let size = bytes.len() as u64;
        let mut file = Cursor::new(bytes);
        let layout = crate::media::container::inspect(&mut file, size)
            .unwrap()
            .unwrap();
        crate::media::audio_source::read_layout(&mut file, size, layout).unwrap()
    }

    #[test]
    fn an_empty_number_or_flag_is_none_as_an_empty_text_is() {
        let names = ["TITLE=Title", "ARTIST=Artist A", "ALBUM=Album"];
        let emptied = flac(
            &[
                &names[..],
                &[
                    "TRACKNUMBER=",
                    "TRACKTOTAL=",
                    "DISCNUMBER=",
                    "DISCTOTAL=",
                    "COMPILATION=",
                    "DATE=",
                    "COMPOSER=",
                ],
            ]
            .concat(),
        );
        let plain = flac(&names);
        assert_eq!(
            (emptied.metadata.track, emptied.metadata.compilation),
            (None, None)
        );
        assert_eq!(project(&emptied).unwrap(), project(&plain).unwrap());
        // A number beside an empty one is the number.
        let numbered = flac(&[&names[..], &["TRACKNUMBER=", "TRACKNUMBER=3"]].concat());
        assert_eq!(numbered.metadata.track, Some(3));
    }

    /// The `tone-aac.m4a` fixture with `edit` made to its tag, read as any
    /// source is.
    fn aac(edit: impl FnOnce(&mut lofty::mp4::Ilst)) -> Source {
        use lofty::{config::WriteOptions, file::AudioFile, mp4::Mp4File};
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tone-aac.m4a"
        ))
        .unwrap();
        let mut file = Cursor::new(bytes);
        let mut mp4 = Mp4File::read_from(&mut file, Default::default()).unwrap();
        edit(mp4.ilst_mut().unwrap());
        mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        let size = file.get_ref().len() as u64;
        let layout = crate::media::container::inspect(&mut file, size)
            .unwrap()
            .unwrap();
        crate::media::audio_source::read_layout(&mut file, size, layout).unwrap()
    }

    #[test]
    fn an_emptied_m4a_atom_projects_as_a_removed_one() {
        use lofty::mp4::{Atom, AtomData, AtomIdent};
        let atoms = [
            *b"aART",
            *b"\xa9wrt",
            *b"cprt",
            *b"\xa9gen",
            *b"\xa9day",
            *b"\xa9cmt",
            *b"sonm",
            *b"soar",
            *b"soco",
        ];
        let removed = project(&aac(|tag| {
            for atom in atoms {
                let _ = tag.remove(&AtomIdent::Fourcc(atom));
            }
        }))
        .unwrap();
        // No bytes, and a bare terminator, which reads as an empty text.
        for empty in ["", "\0"] {
            let emptied = aac(|tag| {
                for atom in atoms {
                    tag.replace_atom(Atom::new(
                        AtomIdent::Fourcc(atom),
                        AtomData::UTF8(empty.into()),
                    ));
                }
            });
            assert_eq!(project(&emptied).unwrap(), removed, "{empty:?}");
        }
        let untitled = aac(|tag| {
            tag.replace_atom(Atom::new(
                AtomIdent::Fourcc(*b"\xa9nam"),
                AtomData::UTF8("\0".into()),
            ));
        });
        assert_eq!(untitled.metadata.title, [""]);
        assert_eq!(project(&untitled).unwrap_err().to_string(), "missing title");
    }

    #[test]
    fn a_flac_source_is_projected_by_its_format() {
        let names = ["TITLE=Title", "ARTIST=Artist A", "ALBUM=Album"];
        let source = flac(&[&names[..], &["COMPOSER=Artist B", "composer=Artist C"]].concat());
        // The reader keeps every entry; the projection joins them.
        assert_eq!(source.metadata.composer, ["Artist B", "Artist C"]);
        assert_eq!(
            project(&source).unwrap().composer.as_deref(),
            Some("Artist B, Artist C")
        );
        let source = flac(&[&names[..], &["artist=Artist B", "ARTIST=Artist C"]].concat());
        assert_eq!(source.metadata.artist, ["Artist A", "Artist B", "Artist C"]);
        assert_eq!(
            project(&source).unwrap().artist,
            "Artist A, Artist B, Artist C"
        );
        let source = flac(&[&names[..], &["TITLE=Other Title"]].concat());
        assert_eq!(
            project(&source).unwrap_err().to_string(),
            "title has 2 values in the file's tag; edit the tag to keep one"
        );
    }
}
