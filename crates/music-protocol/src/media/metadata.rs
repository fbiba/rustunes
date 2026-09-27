//! Read-only local source inspection. These are file properties, not ATC records.

mod source_projected_fields;
pub use source_projected_fields::ProjectedFields;

use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use lofty::{
    config::{ParseOptions, ParsingMode},
    file::AudioFile,
    mp4::{AtomData, AtomIdent, Ilst, Mp4Codec, Mp4File},
    tag::Accessor,
};

// Separate from ATC's wire-frame and AFC readback bounds.
const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

/// Parsed source observations. These fields are not execution permits or ownership claims.
/// Standard-atom provenance remains internal to the parser and its projections.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct Metadata {
    pub title: Vec<String>,
    pub artist: Vec<String>,
    pub album: Vec<String>,
    pub album_artist: Vec<String>,
    pub genre: Vec<String>,
    pub composer: Vec<String>,
    pub copyright: Vec<String>,
    pub release_date: Vec<String>,
    /// Nonempty `©cmt` texts.
    pub comment: Vec<String>,
    /// Explicit sort names: `sonm`, `soar`, `soal`, `soaa`, `soco`.
    pub sort_title: Vec<String>,
    pub sort_artist: Vec<String>,
    pub sort_album: Vec<String>,
    pub sort_album_artist: Vec<String>,
    pub sort_composer: Vec<String>,
    pub compilation: Option<bool>,
    #[serde(skip)]
    standard_atom_counts: BTreeMap<[u8; 4], usize>,
    pub year: Option<u32>,
    pub content_rating: Option<u8>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    // Lofty rounds mdhd duration to milliseconds. Never derive exact sample
    // counts (or an ATC avformat_info.duration) from this display value.
    #[serde(skip)]
    duration: Duration,
    pub timing: crate::media::timing::Timing,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u8,
}

pub fn read(path: &Path) -> Result<Metadata> {
    ensure!(
        path.symlink_metadata()
            .context("reading source path details")?
            .file_type()
            .is_file(),
        "source must be a regular file, not a symlink"
    );
    let file = File::open(path).with_context(|| format!("opening {} read-only", path.display()))?;
    let stat = file.metadata().context("reading source file details")?;
    ensure!(stat.is_file(), "source must be a regular file");
    ensure!(
        stat.len() > 0 && stat.len() <= MAX_SOURCE_BYTES,
        "source must contain 1–{MAX_SOURCE_BYTES} bytes"
    );
    let mut reader = BufReader::new(file);
    let timing = crate::media::timing::read(&mut reader).context("parsing MP4 source timing")?;
    let layout = standard_layout(&mut reader).context("reading standard atom layout")?;
    reader.rewind()?;
    let mp4 =
        Mp4File::read_from(&mut reader, parse_options()).context("parsing MP4 source metadata")?;
    ensure!(
        layout.present
            || mp4
                .ilst()
                .is_none_or(|tag| tag.into_iter().next().is_none()),
        "unqualified standard metadata container path"
    );
    let mut metadata = from_mp4(&mp4, timing)?;
    metadata.standard_atom_counts = layout.counts;
    metadata.compilation = layout.compilation;
    Ok(metadata)
}

/// The standard `ilst` tags of any unprotected MP4 audio file (AAC as well as
/// ALAC), read with [`read`]'s rules, in the shape MPEG tags take, with the
/// ids of every standard atom present.
pub fn mp4_tags(
    reader: &mut (impl Read + Seek),
) -> Result<(
    crate::media::mpeg::Tags,
    Vec<crate::media::mpeg::FrameEvidence>,
)> {
    let layout = standard_layout(reader).context("reading standard atom layout")?;
    reader.rewind()?;
    let mp4 = Mp4File::read_from(reader, parse_options()).context("parsing MP4 metadata")?;
    ensure!(
        layout.present
            || mp4
                .ilst()
                .is_none_or(|tag| tag.into_iter().next().is_none()),
        "unqualified standard metadata container path"
    );
    let empty = Ilst::new();
    let tag = mp4.ilst().unwrap_or(&empty);
    let comments = tag_values(&standard_text(tag, *b"\xa9cmt")?)
        .map(|text| crate::media::mpeg::Comment {
            language: String::new(),
            description: String::new(),
            text: text.to_owned(),
        })
        .collect();
    let tags = crate::media::mpeg::Tags {
        title: standard_text(tag, *b"\xa9nam")?,
        artist: standard_text(tag, *b"\xa9ART")?,
        album: standard_text(tag, *b"\xa9alb")?,
        album_artist: standard_text(tag, *b"aART")?,
        genre: standard_text(tag, *b"\xa9gen")?,
        composer: standard_text(tag, *b"\xa9wrt")?,
        copyright: standard_text(tag, *b"cprt")?,
        release_date: standard_text(tag, *b"\xa9day")?,
        track: tag.track().filter(|number| *number > 0),
        track_total: tag.track_total().filter(|number| *number > 0),
        disc: tag.disk().filter(|number| *number > 0),
        disc_total: tag.disk_total().filter(|number| *number > 0),
        compilation: layout.compilation,
        comments,
        sort_title: standard_text(tag, *b"sonm")?,
        sort_artist: standard_text(tag, *b"soar")?,
        sort_album: standard_text(tag, *b"soal")?,
        sort_album_artist: standard_text(tag, *b"soaa")?,
        sort_composer: standard_text(tag, *b"soco")?,
        encoder_hints: Default::default(),
    };
    let fields = layout
        .counts
        .iter()
        .flat_map(|(name, count)| {
            std::iter::repeat_n(
                crate::media::mpeg::FrameEvidence {
                    id: name.escape_ascii().to_string(),
                    bytes: 0,
                    sha256: String::new(),
                },
                *count,
            )
        })
        .collect();
    Ok((tags, fields))
}

fn parse_options() -> ParseOptions {
    ParseOptions::new()
        .parsing_mode(ParsingMode::Strict)
        .read_cover_art(false)
        // Lofty maps standard numeric gnre to text only if ©gen is absent.
        // This does not promote arbitrary freeform tags to standard fields.
        .implicit_conversions(true)
}

fn from_mp4(mp4: &Mp4File, timing: crate::media::timing::Timing) -> Result<Metadata> {
    let properties = mp4.properties();
    ensure!(
        properties.codec() == &Mp4Codec::ALAC && !properties.is_drm_protected(),
        "source must contain unprotected ALAC audio"
    );
    let sample_rate = properties.sample_rate();
    ensure!(sample_rate > 0, "ALAC sample rate missing");
    ensure!(
        sample_rate == timing.timescale,
        "ALAC readers disagree about sample rate"
    );
    let bit_depth = properties
        .bit_depth()
        .filter(|depth| *depth > 0)
        .context("ALAC bit depth missing")?;
    let channels = properties.channels();
    ensure!(channels > 0, "ALAC channel count missing");
    ensure!(
        !properties.duration().is_zero(),
        "ALAC duration missing or too short"
    );
    let empty = Ilst::new();
    let tag = mp4.ilst().unwrap_or(&empty);
    Ok(Metadata {
        title: standard_text(tag, *b"\xa9nam")?,
        artist: standard_text(tag, *b"\xa9ART")?,
        album: standard_text(tag, *b"\xa9alb")?,
        album_artist: standard_text(tag, *b"aART")?,
        genre: standard_text(tag, *b"\xa9gen")?,
        composer: standard_text(tag, *b"\xa9wrt")?,
        copyright: standard_text(tag, *b"cprt")?,
        release_date: standard_text(tag, *b"\xa9day")?,
        comment: tag_values(&standard_text(tag, *b"\xa9cmt")?)
            .map(str::to_owned)
            .collect(),
        sort_title: standard_text(tag, *b"sonm")?,
        sort_artist: standard_text(tag, *b"soar")?,
        sort_album: standard_text(tag, *b"soal")?,
        sort_album_artist: standard_text(tag, *b"soaa")?,
        sort_composer: standard_text(tag, *b"soco")?,
        compilation: standard_compilation(tag)?,
        standard_atom_counts: atom_counts(tag),
        year: standard_year(tag)?,
        content_rating: standard_rating(tag)?,
        track: tag.track().filter(|number| *number > 0),
        track_total: tag.track_total().filter(|number| *number > 0),
        disc: tag.disk().filter(|number| *number > 0),
        disc_total: tag.disk_total().filter(|number| *number > 0),
        duration: properties.duration(),
        timing,
        sample_rate,
        bit_depth,
        channels,
    })
}

fn standard_year(tag: &Ilst) -> Result<Option<u32>> {
    let dates = standard_text(tag, *b"\xa9day")?;
    let Some(date) = tag_values(&dates).next() else {
        return Ok(None);
    };
    let prefix = date
        .get(..4)
        .context("standard date has no four-digit year")?;
    ensure!(
        prefix.bytes().all(|b| b.is_ascii_digit()),
        "invalid standard date year"
    );
    ensure!(
        date.len() == 4 || date.as_bytes().get(4) == Some(&b'-'),
        "unsupported standard date format"
    );
    let year = prefix.parse::<u32>()?;
    ensure!(year > 0, "zero standard date year");
    Ok(Some(year))
}

fn atom_counts(tag: &Ilst) -> BTreeMap<[u8; 4], usize> {
    let mut counts = BTreeMap::new();
    for atom in tag {
        if let AtomIdent::Fourcc(name) = atom.ident() {
            *counts.entry(*name).or_default() += 1;
        }
    }
    counts
}

fn standard_compilation(tag: &Ilst) -> Result<Option<bool>> {
    let values: Vec<_> = tag
        .into_iter()
        .filter(|atom| atom.ident() == &AtomIdent::Fourcc(*b"cpil"))
        .flat_map(|atom| atom.data())
        .collect();
    ensure!(
        values.len() <= 1,
        "multiple compilation flags are unsupported"
    );
    match values.first() {
        None => Ok(None),
        Some(AtomData::Bool(value)) => Ok(Some(*value)),
        _ => anyhow::bail!("unsupported compilation flag type"),
    }
}

/// The native reader selects data-child index1 for these text fields, and an
/// empty data value is no value ([`tag_value`]), so the first that says
/// something is taken. Raw arrays remain available to inspection; duplicate
/// outer atoms never project.
#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ScalarMetadata<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub album_artist: Option<&'a str>,
    pub genre: Option<&'a str>,
    pub composer: Option<&'a str>,
    pub copyright: Option<&'a str>,
    pub year: Option<u32>,
    pub content_rating: Option<u8>,
    pub compilation: Option<bool>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    /// The source's comment, sent as `comment`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<&'a str>,
    /// Explicit sort tags; without one a sort name is [`default_sort`] of
    /// the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_artist: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album_artist: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_composer: Option<&'a str>,
}

impl<'a> ScalarMetadata<'a> {
    pub fn effective_artist(&self) -> &str {
        self.album_artist.unwrap_or(self.artist)
    }
    /// The sort names the track's insert sends: each explicit sort tag, or
    /// the name without a leading article. A group the track joins keeps its
    /// own; the sender passes that instead.
    pub fn title_sort(&self) -> &'a str {
        self.sort_title.unwrap_or_else(|| default_sort(self.title))
    }
    pub fn artist_sort(&self) -> &'a str {
        self.sort_artist
            .unwrap_or_else(|| default_sort(self.artist))
    }
    pub fn album_sort(&self) -> &'a str {
        self.sort_album.unwrap_or_else(|| default_sort(self.album))
    }
    pub fn album_artist_sort(&self) -> Option<&'a str> {
        self.album_artist
            .map(|name| self.sort_album_artist.unwrap_or_else(|| default_sort(name)))
    }
    pub fn composer_sort(&self) -> Option<&'a str> {
        self.composer
            .map(|name| self.sort_composer.unwrap_or_else(|| default_sort(name)))
    }
}

/// The sort name Apple's sync sends for a name without a sort tag: the name
/// after a leading "The", "An" or "A" and its space, compared without case,
/// when something follows ("The Example Album" sorts as "Example Album", "An
/// Example Title" as "Example Title"); otherwise the name itself. Apple's agent
/// derives it from an article table (`0x140354a58`). "The" and "An" are seen
/// stripped in its traffic; "A" is iTunes' English convention.
pub fn default_sort(name: &str) -> &str {
    let start = name.trim_start_matches(|c: char| c <= ' ');
    for article in ["the ", "an ", "a "] {
        if let Some(head) = start.get(..article.len())
            && head.eq_ignore_ascii_case(article)
        {
            let rest = start[article.len()..].trim_start_matches(' ');
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    name
}

/// A source tag's value as the catalogue takes it. An empty value is no
/// value: Apple's insert omits an empty string (docs/04, section 5.1), so a
/// tag emptied is planned as a tag removed. Every source format's reader and
/// projection take a tag's text through this before their own rule for how
/// many values a field keeps.
pub fn tag_value(value: &str) -> Option<&str> {
    Some(value).filter(|value| !value.is_empty())
}

/// The values of a source tag that say something ([`tag_value`]), in file
/// order.
pub fn tag_values(values: &[String]) -> impl Iterator<Item = &str> {
    values.iter().filter_map(|value| tag_value(value))
}

/// The one comment a source's texts give, if any: identical texts are one
/// comment, and line breaks and tabs may appear in it.
pub fn comment_text<'a>(texts: &[&'a str]) -> Result<Option<&'a str>> {
    let mut distinct: Vec<&'a str> = Vec::new();
    for &text in texts {
        if !distinct.contains(&text) {
            distinct.push(text);
        }
    }
    ensure!(
        distinct.len() <= 1,
        "comment has {} different values in the file's tag; edit the tag to keep one",
        distinct.len()
    );
    let Some(text) = distinct.first().copied() else {
        return Ok(None);
    };
    ensure!(
        text.len() <= 4096
            && !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "invalid comment text"
    );
    Ok(Some(text))
}

impl Metadata {
    /// Compilation scope: canonical true compilation requires one explicit
    /// nonempty album artist. Missing/multiple aART compilation grouping has
    /// not been qualified. This does not authorize any existing-album aggregate
    /// transition; collection planning independently checks that state.
    pub fn ensure_compilation_profile(&self) -> Result<()> {
        if self.compilation == Some(true) {
            let scalar = self.projected()?;
            ensure!(
                tag_values(&self.album_artist).count() == 1
                    && scalar
                        .album_artist
                        .is_some_and(|value| !value.trim().is_empty()),
                "compilation insertion requires one unambiguous nonempty explicit album artist"
            );
        }
        Ok(())
    }

    pub fn duplicate_outer_atoms(&self) -> Vec<String> {
        const SUPPORTED: [[u8; 4]; 19] = [
            *b"\xa9cmt",
            *b"sonm",
            *b"soar",
            *b"soal",
            *b"soaa",
            *b"soco",
            *b"\xa9nam",
            *b"\xa9ART",
            *b"\xa9alb",
            *b"aART",
            *b"\xa9gen",
            *b"gnre",
            *b"\xa9wrt",
            *b"cprt",
            *b"\xa9day",
            *b"rtng",
            *b"cpil",
            *b"trkn",
            *b"disk",
        ];
        self.standard_atom_counts
            .iter()
            .filter(|(name, count)| **count > 1 && SUPPORTED.contains(name))
            .map(|(name, _)| name.iter().map(|b| char::from(*b)).collect())
            .collect()
    }

    pub fn projected(&self) -> Result<ScalarMetadata<'_>> {
        let duplicates = self.duplicate_outer_atoms();
        ensure!(
            duplicates.is_empty(),
            "duplicate outer standard atoms are unprojectable: {duplicates:?}"
        );
        // An empty value is no value: an emptied atom projects as a removed
        // one, and a required field left with none is missing.
        fn first<'a>(values: &'a [String], name: &str) -> Result<Option<&'a str>> {
            let Some(text) = tag_values(values).next() else {
                return Ok(None);
            };
            ensure!(
                text.len() <= 4096 && !text.chars().any(char::is_control),
                "invalid standard {name} text"
            );
            Ok(Some(text))
        }
        Ok(ScalarMetadata {
            title: first(&self.title, "title")?.context("missing standard title")?,
            artist: first(&self.artist, "artist")?.context("missing standard artist")?,
            album: first(&self.album, "album")?.context("missing standard album")?,
            album_artist: first(&self.album_artist, "album artist")?,
            genre: first(&self.genre, "genre")?,
            composer: first(&self.composer, "composer")?,
            copyright: first(&self.copyright, "copyright")?,
            year: self.year,
            content_rating: self.content_rating,
            compilation: self.compilation,
            track: self.track,
            track_total: self.track_total,
            disc: self.disc,
            disc_total: self.disc_total,
            comment: comment_text(&self.comment.iter().map(String::as_str).collect::<Vec<_>>())?,
            sort_title: first(&self.sort_title, "sort title")?,
            sort_artist: first(&self.sort_artist, "sort artist")?,
            sort_album: first(&self.sort_album, "sort album")?,
            sort_album_artist: first(&self.sort_album_artist, "sort album artist")?,
            sort_composer: first(&self.sort_composer, "sort composer")?,
        })
    }
}

#[derive(Default)]
struct StandardLayout {
    counts: BTreeMap<[u8; 4], usize>,
    compilation: Option<bool>,
    present: bool,
}

/// Read only box headers and the bounded cpil body. Lofty's boolean reader
/// deliberately accepts arbitrary integer sizes/values, so it cannot qualify
/// native cpil on its own. Raw counts also survive implicit gnre conversion.
fn standard_layout(source: &mut (impl Read + Seek)) -> Result<StandardLayout> {
    #[derive(Clone, Copy)]
    struct Atom {
        name: [u8; 4],
        body: u64,
        end: u64,
    }
    fn children(source: &mut (impl Read + Seek), mut at: u64, end: u64) -> Result<Vec<Atom>> {
        let mut result = Vec::new();
        ensure!(at <= end, "truncated metadata container");
        while at < end {
            ensure!(
                result.len() < 4096 && end - at >= 8,
                "invalid metadata atom count/header"
            );
            source.seek(SeekFrom::Start(at))?;
            let mut head = [0; 8];
            source.read_exact(&mut head)?;
            let short = u32::from_be_bytes(head[..4].try_into()?);
            let (size, header) = if short == 1 {
                ensure!(end - at >= 16, "truncated extended metadata atom");
                let mut raw = [0; 8];
                source.read_exact(&mut raw)?;
                (u64::from_be_bytes(raw), 16)
            } else {
                (u64::from(short), 8)
            };
            ensure!(
                size >= header && size <= end - at,
                "metadata atom escapes its parent"
            );
            result.push(Atom {
                name: head[4..].try_into()?,
                body: at + header,
                end: at + size,
            });
            at += size;
        }
        Ok(result)
    }
    let end = source.seek(SeekFrom::End(0))?;
    let mut level = children(source, 0, end)?;
    for name in [*b"moov", *b"udta", *b"meta", *b"ilst"] {
        let matches: Vec<_> = level.iter().filter(|a| a.name == name).copied().collect();
        ensure!(matches.len() <= 1, "duplicate metadata containers");
        let Some(atom) = matches.first() else {
            return Ok(StandardLayout::default());
        };
        let skip = if name == *b"meta" { 4 } else { 0 };
        level = children(source, atom.body + skip, atom.end)?;
    }
    let mut result = StandardLayout {
        present: true,
        ..StandardLayout::default()
    };
    for atom in level {
        *result.counts.entry(atom.name).or_default() += 1;
        if atom.name == *b"cpil" {
            ensure!(
                result.compilation.is_none(),
                "multiple compilation flags are unsupported"
            );
            let values = children(source, atom.body, atom.end)?;
            ensure!(
                values.len() == 1
                    && values[0].name == *b"data"
                    && values[0].end - values[0].body == 9,
                "compilation requires one one-byte data value"
            );
            source.seek(SeekFrom::Start(values[0].body))?;
            let mut raw = [0; 9];
            source.read_exact(&mut raw)?;
            ensure!(
                raw[..4] == [0, 0, 0, 21] && raw[4..8] == [0; 4] && raw[8] <= 1,
                "unsupported compilation data type, locale or value"
            );
            result.compilation = Some(raw[8] == 1);
        }
    }
    Ok(result)
}

fn standard_rating(tag: &Ilst) -> Result<Option<u8>> {
    let values: Vec<_> = tag
        .into_iter()
        .filter(|atom| atom.ident() == &AtomIdent::Fourcc(*b"rtng"))
        .flat_map(|atom| atom.data())
        .collect();
    ensure!(
        values.len() <= 1,
        "multiple standard ratings are unsupported"
    );
    let value = match values.first() {
        None => return Ok(None),
        Some(AtomData::SignedInteger(value)) => u8::try_from(*value)?,
        Some(AtomData::UnsignedInteger(value)) => u8::try_from(*value)?,
        _ => anyhow::bail!("unsupported standard rating atom type"),
    };
    ensure!(
        matches!(value, 0 | 1 | 2 | 4),
        "unknown standard advisory rating"
    );
    Ok(Some(value))
}

fn standard_text(tag: &Ilst, fourcc: [u8; 4]) -> Result<Vec<String>> {
    // Intentionally do not merge com.apple.iTunes/TITLE (etc.) into these
    // fields: a standard title edit can leave those freeform copies stale.
    // Preserve repeated values and missing album artist without synthesizing it.
    tag.into_iter()
        .filter(|atom| atom.ident() == &AtomIdent::Fourcc(fourcc))
        .flat_map(|atom| atom.data())
        .map(|value| match value {
            AtomData::UTF8(value) | AtomData::UTF16(value) => Ok(value.clone()),
            _ => anyhow::bail!("standard text atom {fourcc:?} has non-text data"),
        })
        .collect()
}

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Codec: ALAC")?;
        for (name, values) in [
            ("Title", &self.title),
            ("Artist", &self.artist),
            ("Album", &self.album),
            ("Album artist", &self.album_artist),
            ("Genre", &self.genre),
            ("Composer", &self.composer),
            ("Copyright", &self.copyright),
            ("Release date", &self.release_date),
            ("Comment", &self.comment),
            ("Sort title", &self.sort_title),
            ("Sort artist", &self.sort_artist),
            ("Sort album", &self.sort_album),
            ("Sort album artist", &self.sort_album_artist),
            ("Sort composer", &self.sort_composer),
        ] {
            if values.is_empty() {
                writeln!(f, "{name}: (absent)")?;
            }
            for value in values {
                // Escape embedded controls in untrusted local tags.
                writeln!(f, "{name}: {value:?}")?;
            }
        }
        match self.compilation {
            Some(value) => writeln!(f, "Compilation: {value}")?,
            None => writeln!(f, "Compilation: (absent)")?,
        }
        let duplicates = self.duplicate_outer_atoms();
        if !duplicates.is_empty() {
            writeln!(f, "Unprojectable duplicate outer atoms: {duplicates:?}")?;
        }
        for (name, value) in [
            ("Track", self.track),
            ("Track total", self.track_total),
            ("Disc", self.disc),
            ("Disc total", self.disc_total),
            ("Year", self.year),
            ("Content rating", self.content_rating.map(u32::from)),
        ] {
            match value {
                Some(value) => writeln!(f, "{name}: {value}")?,
                None => writeln!(f, "{name}: (absent)")?,
            }
        }
        writeln!(f, "Duration (rounded ms): {}", self.duration.as_millis())?;
        writeln!(f, "Media timescale: {}", self.timing.timescale)?;
        writeln!(f, "Media duration (ticks): {}", self.timing.duration_ticks)?;
        writeln!(
            f,
            "PCM samples/channel (container): {}",
            self.timing.pcm_samples_per_channel()
        )?;
        writeln!(f, "ALAC packets: {}", self.timing.packets)?;
        writeln!(f, "Sample rate (Hz): {}", self.sample_rate)?;
        writeln!(f, "Bit depth: {}", self.bit_depth)?;
        write!(f, "Channels: {}", self.channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::{
        config::WriteOptions,
        mp4::{Atom, DataType},
    };
    use std::{
        fs,
        io::{Cursor, Seek},
        time::{SystemTime, UNIX_EPOCH},
    };

    const ALAC: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/silence-alac.m4a"
    ));

    // Append a separate outer atom, since Lofty's public insert API merges
    // matching atoms and would conceal this reader regression.
    fn append_outer(bytes: &mut Vec<u8>, name: &[u8; 4], data_type: u32, value: &[u8]) {
        fn child(bytes: &[u8], mut at: usize, end: usize, name: &[u8; 4]) -> usize {
            while at < end {
                let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                assert!(size >= 8 && at + size <= end);
                if &bytes[at + 4..at + 8] == name {
                    return at;
                }
                at += size;
            }
            panic!("missing fixture container");
        }
        fn end(bytes: &[u8], at: usize) -> usize {
            at + u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
        }
        let moov = child(bytes, 0, bytes.len(), b"moov");
        let udta = child(bytes, moov + 8, end(bytes, moov), b"udta");
        let meta = child(bytes, udta + 8, end(bytes, udta), b"meta");
        let ilst = child(bytes, meta + 12, end(bytes, meta), b"ilst");
        let mut atom = ((value.len() + 24) as u32).to_be_bytes().to_vec();
        atom.extend(name);
        atom.extend(((value.len() + 16) as u32).to_be_bytes());
        atom.extend(b"data");
        atom.extend(data_type.to_be_bytes());
        atom.extend([0; 4]);
        atom.extend(value);
        let inserted = atom.len() as u32;
        bytes.splice(end(bytes, ilst)..end(bytes, ilst), atom);
        for at in [moov, udta, meta, ilst] {
            let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
            bytes[at..at + 4].copy_from_slice(&(size + inserted).to_be_bytes());
        }
    }

    #[test]
    fn repeated_outer_atoms_preserve_text_order_and_reject_ambiguous_singletons() {
        let mut bytes = ALAC.to_vec();
        append_outer(&mut bytes, b"\xa9ART", 1, b"Second outer artist");
        append_outer(&mut bytes, b"\xa9ART", 1, b"Third outer artist");
        append_outer(&mut bytes, b"\xa9day", 1, b"2025");
        append_outer(&mut bytes, b"\xa9day", 1, b"2026");
        append_outer(&mut bytes, b"rtng", 21, &[1]);
        append_outer(&mut bytes, b"rtng", 21, &[2]);
        let mp4 = Mp4File::read_from(&mut Cursor::new(bytes), parse_options()).unwrap();
        let tag = mp4.ilst().unwrap();
        assert_eq!(
            standard_text(tag, *b"\xa9ART").unwrap(),
            ["First artist", "Second outer artist", "Third outer artist"]
        );
        assert_eq!(standard_year(tag).unwrap(), Some(2025));
        assert!(standard_rating(tag).is_err());
    }

    #[test]
    fn standard_tags_survive_stale_freeform_and_repeated_artists() {
        let mut file = Cursor::new(ALAC.to_vec());
        let mut mp4 = Mp4File::read_from(&mut file, parse_options()).unwrap();
        let tag = mp4.ilst_mut().unwrap();
        tag.set_title("Example [ATC test]".into());
        tag.insert(Atom::new(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "TITLE".into(),
            },
            AtomData::UTF8("Example".into()),
        ));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"\xa9ART"),
            AtomData::UTF8("Second artist".into()),
        ));
        // Round-trip actual atoms through an in-memory file, never user music.
        mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        file.rewind().unwrap();
        let parsed = Mp4File::read_from(&mut file, parse_options()).unwrap();
        let metadata = from_mp4(&parsed, crate::media::timing::read(&mut file).unwrap()).unwrap();
        assert_eq!(metadata.title, ["Example [ATC test]"]);
        assert_eq!(metadata.artist, ["First artist", "Second artist"]);
        assert_eq!(metadata.projected().unwrap().artist, "First artist");
        assert_eq!(metadata.album, ["Example album"]);
        assert!(metadata.album_artist.is_empty());
        assert_eq!(metadata.genre, ["Rap/Hip Hop"]);
        assert_eq!((metadata.track, metadata.track_total), (Some(6), Some(10)));
        assert_eq!((metadata.disc, metadata.disc_total), (Some(1), None));
        let stale = parsed
            .ilst()
            .unwrap()
            .get(&AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "TITLE".into(),
            })
            .unwrap();
        assert!(matches!(stale.data().next(), Some(AtomData::UTF8(value)) if value == "Example"));
    }

    #[test]
    fn identical_comment_values_are_one_comment_and_different_ones_refuse() {
        let read = |values: &[&str]| {
            let mut file = Cursor::new(ALAC.to_vec());
            let mut mp4 = Mp4File::read_from(&mut file, parse_options()).unwrap();
            let tag = mp4.ilst_mut().unwrap();
            for value in values {
                tag.insert(Atom::new(
                    AtomIdent::Fourcc(*b"\xa9cmt"),
                    AtomData::UTF8((*value).into()),
                ));
            }
            mp4.save_to(&mut file, WriteOptions::default()).unwrap();
            file.rewind().unwrap();
            let parsed = Mp4File::read_from(&mut file, parse_options()).unwrap();
            from_mp4(&parsed, crate::media::timing::read(&mut file).unwrap()).unwrap()
        };
        let same = read(&["One comment", "One comment"]);
        assert_eq!(same.comment, ["One comment", "One comment"]);
        assert_eq!(
            same.projected().unwrap().comment.as_deref(),
            Some("One comment")
        );
        let different = read(&["One comment", "Another"]);
        assert!(
            different
                .projected()
                .unwrap_err()
                .to_string()
                .contains("2 different values")
        );
    }

    /// `bytes` read as [`read`] reads a file, in memory.
    fn parsed(bytes: Vec<u8>) -> Result<Metadata> {
        let mut file = Cursor::new(bytes);
        let timing = crate::media::timing::read(&mut file)?;
        let layout = standard_layout(&mut file)?;
        file.rewind()?;
        let mp4 = Mp4File::read_from(&mut file, parse_options())?;
        let mut metadata = from_mp4(&mp4, timing)?;
        metadata.standard_atom_counts = layout.counts;
        metadata.compilation = layout.compilation;
        Ok(metadata)
    }

    /// The fixture with `edit` made to its tag, read as [`read`] reads it.
    fn retagged(edit: impl FnOnce(&mut Ilst)) -> Result<Metadata> {
        let mut file = Cursor::new(ALAC.to_vec());
        let mut mp4 = Mp4File::read_from(&mut file, parse_options()).unwrap();
        edit(mp4.ilst_mut().unwrap());
        mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        parsed(file.into_inner())
    }

    /// The ways a data value can say nothing: no bytes (Lofty skips such a
    /// value, so its atom reads as absent), a bare terminator, and a UTF-16
    /// byte-order mark alone; the last two read as an empty text.
    fn empty_values() -> [AtomData; 3] {
        [
            AtomData::UTF8(String::new()),
            AtomData::UTF8("\0".into()),
            AtomData::Unknown {
                code: DataType::Utf16,
                data: vec![0xfe, 0xff],
            },
        ]
    }

    #[test]
    fn an_emptied_atom_projects_as_a_removed_one() {
        let optional = [
            *b"aART",
            *b"\xa9gen",
            *b"\xa9wrt",
            *b"cprt",
            *b"\xa9day",
            *b"\xa9cmt",
            *b"sonm",
            *b"soar",
            *b"soal",
            *b"soaa",
            *b"soco",
        ];
        let removed = retagged(|tag| {
            for atom in optional {
                let _ = tag.remove(&AtomIdent::Fourcc(atom));
            }
        })
        .unwrap();
        for empty in empty_values() {
            let emptied = retagged(|tag| {
                for atom in optional {
                    tag.replace_atom(Atom::new(AtomIdent::Fourcc(atom), empty.clone()));
                }
            })
            .unwrap();
            let s = emptied.projected().unwrap();
            assert_eq!(s, removed.projected().unwrap(), "{empty:?}");
            assert_eq!(
                (s.album_artist, s.genre, s.composer, s.copyright, s.year),
                (None, None, None, None, None)
            );
            assert_eq!(
                (s.comment, s.sort_title, s.sort_composer),
                (None, None, None)
            );
        }
        // A terminator alone reads as an empty text, which inspection keeps.
        let kept = retagged(|tag| {
            tag.replace_atom(Atom::new(
                AtomIdent::Fourcc(*b"\xa9wrt"),
                AtomData::UTF8("\0".into()),
            ));
        })
        .unwrap();
        assert_eq!(kept.composer, [""]);
        assert!(kept.to_string().contains("Composer: \"\"\n"));
        // A required field left with no value is missing, as a removed one is.
        for (atom, field) in [
            (*b"\xa9nam", "title"),
            (*b"\xa9ART", "artist"),
            (*b"\xa9alb", "album"),
        ] {
            let missing = format!("missing standard {field}");
            let removed = retagged(|tag| {
                let _ = tag.remove(&AtomIdent::Fourcc(atom));
            })
            .unwrap();
            assert_eq!(removed.projected().unwrap_err().to_string(), missing);
            for empty in empty_values() {
                let emptied =
                    retagged(|tag| tag.replace_atom(Atom::new(AtomIdent::Fourcc(atom), empty)))
                        .unwrap();
                assert_eq!(emptied.projected().unwrap_err().to_string(), missing);
            }
        }
        // A compilation whose album artist is emptied is refused as one
        // without an album artist.
        let compilation = |album_artist: Option<AtomData>| {
            retagged(|tag| {
                tag.insert(Atom::new(AtomIdent::Fourcc(*b"cpil"), AtomData::Bool(true)));
                if let Some(value) = album_artist {
                    tag.replace_atom(Atom::new(AtomIdent::Fourcc(*b"aART"), value));
                }
            })
            .unwrap()
            .ensure_compilation_profile()
            .unwrap_err()
            .to_string()
        };
        assert_eq!(
            compilation(Some(AtomData::UTF8("\0".into()))),
            compilation(None)
        );
    }

    #[test]
    fn the_first_value_that_says_something_is_taken_and_checked() {
        let composer = AtomIdent::Fourcc(*b"\xa9wrt");
        let sort = AtomIdent::Fourcc(*b"soco");
        // One atom holding an empty data value, then one that says something.
        let m = retagged(|tag| {
            for ident in [&composer, &sort] {
                tag.replace_atom(Atom::new(ident.clone(), AtomData::UTF8("\0".into())));
                tag.insert(Atom::new(ident.clone(), AtomData::UTF8("Artist B".into())));
            }
        })
        .unwrap();
        assert_eq!(m.composer, ["", "Artist B"]);
        let s = m.projected().unwrap();
        assert_eq!(
            (s.composer, s.sort_composer),
            (Some("Artist B"), Some("Artist B"))
        );
        // A value that says something is held to the text rules as before.
        let refused = |value: &str| {
            retagged(|tag| {
                tag.replace_atom(Atom::new(composer.clone(), AtomData::UTF8(value.into())));
            })
            .unwrap()
            .projected()
            .unwrap_err()
            .to_string()
        };
        assert_eq!(refused("Artist\tB"), "invalid standard composer text");
        assert_eq!(refused(&"a".repeat(4097)), "invalid standard composer text");
        // An empty atom beside another of its name is still a second outer atom.
        let mut bytes = ALAC.to_vec();
        append_outer(&mut bytes, b"\xa9ART", 1, b"\0");
        let error = parsed(bytes).unwrap().projected().unwrap_err();
        assert!(error.to_string().contains("duplicate outer"), "{error:#}");
    }

    #[test]
    fn duplicate_outer_text_is_inspectable_but_not_projectable() {
        let mut bytes = ALAC.to_vec();
        append_outer(&mut bytes, b"\xa9ART", 1, b"Second outer artist");
        let mut file = Cursor::new(bytes);
        let layout = standard_layout(&mut file).unwrap();
        file.rewind().unwrap();
        let parsed = Mp4File::read_from(&mut file, parse_options()).unwrap();
        let mut metadata =
            from_mp4(&parsed, crate::media::timing::read(&mut file).unwrap()).unwrap();
        metadata.standard_atom_counts = layout.counts;
        assert_eq!(metadata.artist, ["First artist", "Second outer artist"]);
        assert_eq!(metadata.duplicate_outer_atoms(), ["©ART"]);
        assert!(
            metadata
                .projected()
                .unwrap_err()
                .to_string()
                .contains("duplicate outer")
        );
    }

    #[test]
    fn raw_compilation_requires_one_canonical_zero_or_one_flag() {
        assert_eq!(
            standard_layout(&mut Cursor::new(ALAC)).unwrap().compilation,
            None
        );
        for value in [0, 1] {
            let mut bytes = ALAC.to_vec();
            append_outer(&mut bytes, b"cpil", 21, &[value]);
            assert_eq!(
                standard_layout(&mut Cursor::new(bytes))
                    .unwrap()
                    .compilation,
                Some(value == 1)
            );
        }
        for (typ, value) in [(21, vec![2]), (1, vec![1]), (21, vec![0, 1]), (21, vec![])] {
            let mut bytes = ALAC.to_vec();
            append_outer(&mut bytes, b"cpil", typ, &value);
            assert!(standard_layout(&mut Cursor::new(bytes)).is_err());
        }
        let mut bytes = ALAC.to_vec();
        append_outer(&mut bytes, b"cpil", 21, &[0]);
        append_outer(&mut bytes, b"cpil", 21, &[1]);
        assert!(standard_layout(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn numeric_genre_is_used_only_when_custom_text_genre_is_absent() {
        for custom in [true, false] {
            let mut file = Cursor::new(ALAC.to_vec());
            let mut mp4 = Mp4File::read_from(&mut file, parse_options()).unwrap();
            let tag = mp4.ilst_mut().unwrap();
            tag.insert(Atom::new(
                AtomIdent::Fourcc(*b"gnre"),
                AtomData::Unknown {
                    code: DataType::Reserved,
                    // MP4's standard numeric genre is one-based: 18 is Rock.
                    data: vec![0, 18],
                },
            ));
            if !custom {
                tag.remove_genre();
            }
            mp4.save_to(&mut file, WriteOptions::default()).unwrap();
            file.rewind().unwrap();
            let parsed = Mp4File::read_from(&mut file, parse_options()).unwrap();
            let metadata =
                from_mp4(&parsed, crate::media::timing::read(&mut file).unwrap()).unwrap();
            assert_eq!(
                metadata.genre,
                [if custom { "Rap/Hip Hop" } else { "Rock" }]
            );
        }
    }

    #[test]
    fn reads_handcrafted_utf16_atom_and_escapes_display_controls() {
        let mut file = Cursor::new(ALAC.to_vec());
        let mut mp4 = Mp4File::read_from(&mut file, parse_options()).unwrap();
        let title = "Καλημέρα 🎵\n";
        mp4.ilst_mut().unwrap().replace_atom(Atom::new(
            AtomIdent::Fourcc(*b"\xa9nam"),
            AtomData::Unknown {
                code: DataType::Utf16,
                // Handcrafted type-2 bytes: avoid Lofty 0.22.4's broken
                // UTF16 text writer. Production never calls a tag writer.
                data: title.encode_utf16().flat_map(u16::to_be_bytes).collect(),
            },
        ));
        mp4.save_to(&mut file, WriteOptions::default()).unwrap();
        file.rewind().unwrap();
        let parsed = Mp4File::read_from(&mut file, parse_options()).unwrap();
        let metadata = from_mp4(&parsed, crate::media::timing::read(&mut file).unwrap()).unwrap();
        assert_eq!(metadata.title, [title]);
        assert!(metadata.to_string().contains("Title: \"Καλημέρα 🎵\\n\"\n"));
    }

    #[test]
    fn local_reader_reports_alac_properties_and_refuses_aac() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let metadata = read(&root.join("silence-alac.m4a")).unwrap();
        assert_eq!(metadata.duration, Duration::from_millis(50));
        assert_eq!(metadata.sample_rate, 44100);
        assert_eq!(metadata.bit_depth, 16);
        assert_eq!(metadata.channels, 2);
        let error = read(&root.join("silence-aac.m4a")).unwrap_err();
        assert!(format!("{error:#}").contains("must contain unprotected ALAC"));
    }

    #[test]
    fn missing_standard_text_stays_absent_and_non_text_is_rejected() {
        let mut tag = Ilst::new();
        tag.insert(Atom::new(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "TITLE".into(),
            },
            AtomData::UTF8("Unverified freeform title".into()),
        ));
        assert!(standard_text(&tag, *b"\xa9nam").unwrap().is_empty());
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"\xa9nam"),
            AtomData::SignedInteger(42),
        ));
        assert!(standard_text(&tag, *b"\xa9nam").is_err());
    }

    #[test]
    fn standard_release_date_and_rating_keep_native_values_without_freeform_fallback() {
        let mut tag = Ilst::new();
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"\xa9day"),
            AtomData::UTF8("2026-03-19".into()),
        ));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"rtng"),
            AtomData::SignedInteger(1),
        ));
        assert_eq!(standard_year(&tag).unwrap(), Some(2026));
        assert_eq!(standard_rating(&tag).unwrap(), Some(1));
        tag.replace_atom(Atom::new(
            AtomIdent::Fourcc(*b"rtng"),
            AtomData::SignedInteger(-1),
        ));
        assert!(standard_rating(&tag).is_err());
        tag.replace_atom(Atom::new(
            AtomIdent::Fourcc(*b"\xa9day"),
            AtomData::UTF8("2026arbitrary".into()),
        ));
        assert!(standard_year(&tag).is_err());
        let empty = Ilst::new();
        assert_eq!(standard_year(&empty).unwrap(), None);
        assert_eq!(standard_rating(&empty).unwrap(), None);
    }

    #[test]
    fn rejects_empty_truncated_and_oversized_files_without_modifying_them() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "music-protocol-metadata-{}-{nonce}.m4a",
            std::process::id()
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        assert!(
            read(&path)
                .unwrap_err()
                .to_string()
                .contains("source must contain")
        );
        use std::io::Write;
        file.write_all(&ALAC[..32]).unwrap();
        assert!(read(&path).unwrap_err().to_string().contains("parsing MP4"));
        assert_eq!(fs::read(&path).unwrap(), ALAC[..32]);
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        assert!(
            read(&path)
                .unwrap_err()
                .to_string()
                .contains("source must contain")
        );
        assert_eq!(file.metadata().unwrap().len(), MAX_SOURCE_BYTES + 1);
        drop(file);
        fs::remove_file(&path).unwrap();
        assert!(
            read(&std::env::temp_dir())
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
    }
}
