//! Read-only local source inspection. These are file properties, not ATC records.

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
