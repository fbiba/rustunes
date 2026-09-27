//! Bounded source-side MPEG-1 Layer III and ID3 inspection, never insertion authority.
//! Encoded identity hashes the actual complete frame span, including Xing/LAME
//! frames. Frame samples are a coded duration, not a gapless playback promise.
//! [`read`] takes every other non-ALAC format too: [`audio_source`] fills the
//! same [`Source`] for them, with a stream in place of frame timing.
//!
//! [`audio_source`]: crate::media::audio_source
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub mod gapless;
pub mod projection;
pub mod timing;

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TAG: usize = 16 * 1024 * 1024;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fingerprint {
    pub file_bytes: u64,
    pub full_file_sha256: String,
    pub audio: EncodedMpeg,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EncodedMpeg {
    pub format: &'static str,
    pub sha256: String,
    pub frame_bytes_sha256: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_count: u32,
    pub audio_bytes: u64,
    pub coded_samples: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tags {
    pub title: Vec<String>,
    pub artist: Vec<String>,
    pub album: Vec<String>,
    pub album_artist: Vec<String>,
    pub genre: Vec<String>,
    pub composer: Vec<String>,
    pub copyright: Vec<String>,
    pub release_date: Vec<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    /// COMM frames that say something. A frame with an empty description and
    /// empty text is left out: it has nothing a catalogue could hold.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<Comment>,
    /// Explicit sort names: ID3 TSOT, TSOP, TSOA, TSO2 and TSOC, MP4 sonm,
    /// soar, soal, soaa and soco, and the Vorbis `*SORT` comments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_title: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_artist: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_album: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_album_artist: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort_composer: Vec<String>,
    /// What Apple's sync reads from an ID3v2 tag for an MP3's gapless fields.
    #[serde(skip_serializing_if = "gapless::EncoderHints::is_empty")]
    pub encoder_hints: gapless::EncoderHints,
}
impl Tags {
    /// The distinct texts a catalogue comment can come from: comments
    /// without a description. Apple's sync takes an ID3 comment only from a
    /// COMM frame with an empty description, in any language; a described
    /// frame (iTunNORM, iTunSMPB and the like) holds other data.
    pub fn catalogue_comments(&self) -> Vec<&str> {
        let mut texts: Vec<&str> = Vec::new();
        for comment in &self.comments {
            if comment.description.is_empty()
                && !comment.text.is_empty()
                && !texts.contains(&comment.text.as_str())
            {
                texts.push(&comment.text);
            }
        }
        texts
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Comment {
    pub language: String,
    pub description: String,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FrameEvidence {
    pub id: String,
    pub bytes: usize,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Picture {
    /// What the bytes are, whatever the container declared.
    pub mime: &'static str,
    /// The container declared an image type (see [`image_facts`]), as FFmpeg
    /// needs to read a FLAC's picture; not in the JSON.
    #[serde(skip)]
    pub declared_type: bool,
    pub picture_type: u8,
    pub description: String,
    pub bytes: usize,
    pub sha256: String,
    pub width: u16,
    pub height: u16,
    /// The JPEG's markers or the PNG's chunks were checked (not a decode).
    pub structure_verified: bool,
    pub image_decode_verified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Source {
    /// The format's name (`mpeg1-layer3`, `aac-lc`, `flac`, ...).
    pub codec: &'static str,
    pub fingerprint: Fingerprint,
    /// MPEG-1 Layer III frame timing; `None` for every other format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<timing::TimingEvidence>,
    /// Every other format's stream; `None` for MPEG-1 Layer III.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<crate::media::audio_source::Stream>,
    /// The timing and gapless fields Apple's sync sends for an MPEG Layer III
    /// stream; `None` for every other format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apple_timing: Option<gapless::AppleTiming>,
    pub metadata: Tags,
    pub id3_version: Option<u8>,
    pub frames: Vec<FrameEvidence>,
    pub artwork: Vec<Picture>,
    pub audio_offset: u64,
    pub trailing_tag_bytes: u32,
}
impl Source {
    pub fn format(&self) -> crate::media::format::Format {
        self.stream
            .as_ref()
            .map_or(crate::media::format::Format::Mpeg1Layer3, |s| s.format)
    }
    /// The frame timing of an MPEG-1 Layer III source.
    pub fn frame_timing(&self) -> Result<&timing::TimingEvidence> {
        self.timing
            .as_ref()
            .context("the source is not MPEG-1 Layer III")
    }
    pub fn transferred_identity(&self) -> Result<crate::media::fingerprint::TransferredSource> {
        fn digest(s: &str) -> Result<[u8; 32]> {
            ensure!(
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid MP3 digest"
            );
            let mut out = [0; 32];
            for (i, b) in out.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
            }
            Ok(out)
        }
        if let Some(stream) = &self.stream {
            ensure!(
                self.codec == stream.format.name()
                    && self.timing.is_none()
                    && self.fingerprint.audio.format == "encoded-audio",
                "audio inspection identity differs"
            );
            let value = crate::media::fingerprint::TransferredSource::encoded(
                self.fingerprint.file_bytes,
                digest(&self.fingerprint.full_file_sha256)?,
                stream.encoded.clone(),
            )?;
            ensure!(
                value.audio.sha256 == digest(&self.fingerprint.audio.sha256)?,
                "audio report and identity differ"
            );
            return Ok(value);
        }
        ensure!(
            self.codec == "mpeg1-layer3"
                && self.fingerprint.audio.format == "encoded-mpeg1-layer3"
                && self.fingerprint.audio.coded_samples
                    == u64::from(self.fingerprint.audio.frame_count) * 1152,
            "MP3 inspection identity differs"
        );
        let a = &self.fingerprint.audio;
        let value = crate::media::fingerprint::TransferredSource::mpeg(
            self.fingerprint.file_bytes,
            digest(&self.fingerprint.full_file_sha256)?,
            crate::media::fingerprint::EncodedMpeg1Layer3 {
                sample_rate: a.sample_rate,
                channels: a.channels,
                frame_count: a.frame_count,
                audio_offset: self.audio_offset,
                audio_bytes: a.audio_bytes,
                trailing_bytes: u64::from(self.trailing_tag_bytes),
                frame_bytes_sha256: digest(&a.frame_bytes_sha256)?,
            },
        )?;
        ensure!(
            value.audio.sha256 == digest(&a.sha256)?,
            "MP3 report/frame identity differs"
        );
        Ok(value)
    }
}
pub fn is_mp3(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("mp3"))
}
/// Whether [`read`] takes `path` by its extension alone: MP3, FLAC, WAV and
/// AIFF. An `.m4a` may be ALAC, which [`read`] refuses.
pub fn is_other_audio(path: &Path) -> bool {
    path.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        ["mp3", "flac", "wav", "wave", "aif", "aiff", "aifc"]
            .iter()
            .any(|e| x.eq_ignore_ascii_case(e))
    })
}
use crate::media::held_source::same_file as same;
fn hash_span(file: &mut File, start: u64, bytes: u64) -> Result<[u8; 32]> {
    file.seek(SeekFrom::Start(start))?;
    let mut left = bytes;
    let mut buffer = [0; 65536];
    let mut h = Sha256::new();
    while left > 0 {
        let count = usize::try_from(left.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        h.update(&buffer[..count]);
        left -= count as u64;
    }
    Ok(h.finalize().into())
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn syncsafe(b: &[u8]) -> Result<usize> {
    ensure!(
        b.len() == 4 && b.iter().all(|v| v & 128 == 0),
        "invalid ID3 syncsafe size"
    );
    Ok(b.iter().fold(0, |n, v| (n << 7) | usize::from(*v)))
}
pub(crate) type Parsed = (Tags, Option<u8>, Vec<FrameEvidence>, Vec<Picture>);
pub(crate) fn parse_id3(bytes: &[u8]) -> Result<Parsed> {
    parse_id3_with_picture(bytes, &mut |_, _| Ok(()))
}
pub(crate) fn parse_id3_with_picture(
    bytes: &[u8],
    on_picture: &mut impl FnMut(&Picture, &[u8]) -> Result<()>,
) -> Result<Parsed> {
    let mut tags = Tags::default();
    let mut evidence = Vec::new();
    let mut pictures = Vec::new();
    if bytes.is_empty() {
        return Ok((tags, None, evidence, pictures));
    }
    ensure!(
        bytes.len() >= 10 && &bytes[..3] == b"ID3" && matches!(bytes[3], 3 | 4) && bytes[4] == 0,
        "unsupported ID3 version"
    );
    // Refuse unsynchronisation, extended headers, encryption/compression and
    // experimental layouts rather than treating encoded bytes as metadata.
    ensure!(
        bytes[5] == 0 || (bytes[3] == 4 && bytes[5] == 0x10),
        "unsupported ID3 header flags"
    );
    let version = bytes[3];
    let end = 10 + syncsafe(&bytes[6..10])?;
    ensure!(
        end <= bytes.len() && bytes.len() == end + if bytes[5] & 0x10 != 0 { 10 } else { 0 },
        "ID3 span mismatch"
    );
    let mut offset = 10;
    let mut seen = BTreeMap::<String, usize>::new();
    let mut text_total = 0usize;
    while offset < end {
        if bytes[offset] == 0 {
            ensure!(
                bytes[offset..end].iter().all(|b| *b == 0),
                "nonzero ID3 padding"
            );
            break;
        }
        ensure!(
            evidence.len() < 4096 && end - offset >= 10,
            "ID3 frame count/header bound"
        );
        let id = &bytes[offset..offset + 4];
        ensure!(
            id.iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "invalid ID3 frame ID"
        );
        let id = std::str::from_utf8(id)?.to_owned();
        let len = if version == 4 {
            syncsafe(&bytes[offset + 4..offset + 8])?
        } else {
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize
        };
        ensure!(
            len > 0 && len <= end - offset - 10,
            "invalid ID3 frame extent"
        );
        ensure!(
            bytes[offset + 8..offset + 10] == [0, 0],
            "unsupported ID3 frame flags"
        );
        let data = &bytes[offset + 10..offset + 10 + len];
        evidence.push(FrameEvidence {
            id: id.clone(),
            bytes: len,
            sha256: hex(&Sha256::digest(data)),
        });
        *seen.entry(id.clone()).or_default() += 1;
        let target = match id.as_str() {
            "TIT2" => Some(&mut tags.title),
            "TPE1" => Some(&mut tags.artist),
            "TALB" => Some(&mut tags.album),
            "TPE2" => Some(&mut tags.album_artist),
            "TCON" => Some(&mut tags.genre),
            "TCOM" => Some(&mut tags.composer),
            "TCOP" => Some(&mut tags.copyright),
            "TDRC" | "TYER" => Some(&mut tags.release_date),
            "TSOT" => Some(&mut tags.sort_title),
            "TSOP" => Some(&mut tags.sort_artist),
            "TSOA" => Some(&mut tags.sort_album),
            "TSO2" => Some(&mut tags.sort_album_artist),
            "TSOC" => Some(&mut tags.sort_composer),
            _ => None,
        };
        if let Some(target) = target {
            let values = text_frame(data, version)?;
            text_total += values.iter().map(String::len).sum::<usize>();
            target.extend(values);
            ensure!(
                target.len() <= 32 && text_total <= MAX_TEXT,
                "ID3 text exceeds bound"
            );
        } else if matches!(id.as_str(), "TRCK" | "TPOS" | "TCMP") {
            ensure!(seen[&id] == 1, "duplicate ID3 numeric frame");
            let values = text_frame(data, version)?;
            // An empty number is no number, as an empty text is no text.
            let values: Vec<_> = crate::media::metadata::tag_values(&values).collect();
            ensure!(values.len() <= 1, "ambiguous ID3 numeric frame");
            if let Some(&value) = values.first() {
                if id == "TCMP" {
                    tags.compilation = Some(match value {
                        "0" => false,
                        "1" => true,
                        _ => anyhow::bail!("invalid ID3 compilation"),
                    });
                } else {
                    let (number, total) = number_pair(value)?;
                    if id == "TRCK" {
                        tags.track = Some(number);
                        tags.track_total = total;
                    } else {
                        tags.disc = Some(number);
                        tags.disc_total = total;
                    }
                }
            }
        } else if matches!(id.as_str(), "TENC" | "TSSE") {
            // Only Apple's gapless rule reads these; one it cannot decode
            // names no encoder.
            if let Some(text) = text_frame(data, version)
                .ok()
                .and_then(|v| v.into_iter().next())
            {
                tags.encoder_hints.encoder(&text);
            }
        } else if id == "COMM" {
            if let Some(comment) = comment_frame(data, version)? {
                tags.encoder_hints
                    .comment(&comment.description, &comment.text);
                text_total += comment.description.len() + comment.text.len();
                tags.comments.push(comment);
                ensure!(
                    tags.comments.len() <= 32 && text_total <= MAX_TEXT,
                    "ID3 comments exceed bound"
                );
            }
        } else if id == "APIC" {
            ensure!(pictures.len() < 8, "too many ID3 pictures");
            let (facts, image) = picture_payload(data, version)?;
            on_picture(&facts, image)?;
            pictures.push(facts);
        }
        offset += 10 + len;
    }
    Ok((tags, Some(version), evidence, pictures))
}
/// Encoding, three-byte language, a terminated description, then the text.
/// None when both description and text are empty.
fn comment_frame(data: &[u8], version: u8) -> Result<Option<Comment>> {
    ensure!(
        data.len() >= 4 && data.len() <= MAX_TEXT,
        "ID3 comment size bound"
    );
    let (encoding, language, rest) = (data[0], &data[1..4], &data[4..]);
    ensure!(
        language.iter().all(|b| b.is_ascii_alphabetic()) || language == [0, 0, 0],
        "invalid ID3 comment language"
    );
    let width = if matches!(encoding, 1 | 2) { 2 } else { 1 };
    let terminator = rest
        .chunks_exact(width)
        .position(|c| c.iter().all(|b| *b == 0))
        .context("ID3 comment description terminator missing")?
        * width;
    let decode = |bytes: &[u8]| -> Result<String> {
        if bytes.is_empty() {
            ensure!(
                encoding <= 3 && (version == 4 || encoding <= 1),
                "invalid ID3 comment encoding"
            );
            return Ok(String::new());
        }
        decode_text(encoding, bytes, version)
    };
    let description = decode(&rest[..terminator])?;
    let text = decode(&rest[terminator + width..])?
        .trim_end_matches('\0')
        .to_owned();
    ensure!(
        !description.contains('\0') && !text.contains('\0'),
        "ambiguous ID3 comment text"
    );
    if description.is_empty() && text.is_empty() {
        return Ok(None);
    }
    Ok(Some(Comment {
        language: language.iter().map(|b| char::from(*b)).collect(),
        description,
        text,
    }))
}
pub(crate) fn number_pair(s: &str) -> Result<(u32, Option<u32>)> {
    let mut p = s.split('/');
    let number = p.next().context("empty numeric tag")?.parse()?;
    let total = p.next().map(str::parse).transpose()?;
    ensure!(p.next().is_none(), "invalid numeric pair");
    Ok((number, total))
}
fn text_frame(data: &[u8], version: u8) -> Result<Vec<String>> {
    ensure!(
        !data.is_empty() && data.len() <= MAX_TEXT,
        "ID3 text size bound"
    );
    let text = decode_text(data[0], &data[1..], version)?;
    let values: Vec<_> = text
        .trim_end_matches('\0')
        .split('\0')
        .map(str::to_owned)
        .collect();
    ensure!(values.len() <= 32, "ID3 text value bound");
    Ok(values)
}
fn decode_text(encoding: u8, data: &[u8], version: u8) -> Result<String> {
    ensure!(
        version == 4 || encoding <= 1,
        "ID3v2.3 text encoding unsupported"
    );
    let s = match encoding {
        0 => data.iter().map(|v| char::from(*v)).collect(),
        3 => std::str::from_utf8(data)?.to_owned(),
        1 | 2 => {
            let (le, raw) = if encoding == 1 {
                ensure!(data.len() >= 2, "missing UTF16 BOM");
                match &data[..2] {
                    [0xff, 0xfe] => (true, &data[2..]),
                    [0xfe, 0xff] => (false, &data[2..]),
                    _ => anyhow::bail!("invalid UTF16 BOM"),
                }
            } else {
                (false, data)
            };
            ensure!(raw.len() % 2 == 0, "odd UTF16 span");
            let words: Vec<u16> = raw
                .chunks_exact(2)
                .map(|b| {
                    if le {
                        u16::from_le_bytes([b[0], b[1]])
                    } else {
                        u16::from_be_bytes([b[0], b[1]])
                    }
                })
                .collect();
            String::from_utf16(&words)?
        }
        _ => anyhow::bail!("unknown ID3 text encoding"),
    };
    ensure!(
        s.len() <= MAX_TEXT
            && !s.chars().any(|c| c.is_control()
                && c != '\0'
                && c != '\t'
                && c != '\n'
                && c != '\r'),
        "invalid or overlong ID3 text"
    );
    Ok(s)
}
fn picture_payload(data: &[u8], version: u8) -> Result<(Picture, &[u8])> {
    ensure!(data.len() > 5, "truncated APIC");
    let encoding = data[0];
    let end = data[1..]
        .iter()
        .position(|b| *b == 0)
        .context("APIC MIME terminator missing")?
        + 1;
    let mime = &data[1..end];
    let kind = *data.get(end + 1).context("APIC type missing")?;
    ensure!(kind <= 20, "invalid APIC type");
    let rest = &data[end + 2..];
    let width = if encoding == 1 || encoding == 2 { 2 } else { 1 };
    let terminator = rest
        .chunks_exact(width)
        .position(|c| c.iter().all(|b| *b == 0))
        .context("APIC description terminator missing")?
        * width;
    let description = if terminator == 0 {
        ensure!(
            encoding <= 3 && (version == 4 || encoding <= 1),
            "invalid APIC encoding"
        );
        String::new()
    } else {
        decode_text(encoding, &rest[..terminator], version)?
    };
    let image = &rest[terminator + width..];
    let (held, (w, h)) = image_facts(mime, image)?;
    Ok((
        Picture {
            mime: held,
            declared_type: declares_type(mime),
            picture_type: kind,
            description,
            bytes: image.len(),
            sha256: hex(&Sha256::digest(image)),
            width: w,
            height: h,
            structure_verified: true,
            image_decode_verified: false,
        },
        image,
    ))
}

/// Whether a declared picture type names a type, rather than being empty or a
/// bare `image/`.
pub(crate) fn declares_type(mime: &[u8]) -> bool {
    !matches!(mime.to_ascii_lowercase().as_slice(), b"" | b"image/")
}

/// What a picture's bytes are: a JPEG of any kind or a PNG, the two cover types
/// Apple's sync sends unchanged, or a GIF, BMP or TIFF, which it converts (and
/// which sync sends as a JPEG, [`bitmap::to_jpeg`]). A declared type must agree
/// with the bytes. An empty type, or a bare `image/`, declares nothing (ID3v2.3
/// reads an omitted one as `image/`, and taggers leave it empty in FLAC): the
/// bytes alone decide, and still have to be one of those types.
///
/// [`bitmap::to_jpeg`]: crate::media::bitmap::to_jpeg
pub(crate) fn image_facts(mime: &[u8], image: &[u8]) -> Result<(&'static str, (u16, u16))> {
    use crate::media::bitmap::Format;
    let declared = match mime.to_ascii_lowercase().as_slice() {
        _ if !declares_type(mime) => None,
        b"image/jpeg" | b"image/jpg" => Some("image/jpeg"),
        b"image/png" => Some("image/png"),
        b"image/gif" => Some("image/gif"),
        b"image/bmp" | b"image/x-bmp" | b"image/x-ms-bmp" => Some("image/bmp"),
        b"image/tiff" => Some("image/tiff"),
        _ => anyhow::bail!(
            "only JPEG, PNG, GIF, BMP and TIFF pictures are read, not {:?}",
            String::from_utf8_lossy(mime)
        ),
    };
    let held = if image.starts_with(&crate::media::png::SIGNATURE) {
        "image/png"
    } else {
        Format::of(image).map_or("image/jpeg", Format::mime)
    };
    ensure!(
        declared.is_none_or(|declared| declared == held),
        "a picture declared {:?} holds other image data",
        String::from_utf8_lossy(mime)
    );
    Ok((
        held,
        match held {
            "image/png" => crate::media::png::dimensions(image)?,
            "image/jpeg" => crate::media::jpeg::source_dimensions(image)?,
            _ => crate::media::bitmap::dimensions(
                image,
                Format::of(image).context("picture format")?,
            )?,
        },
    ))
}

/// Which of a source's pictures sync sends as its cover: the one picture
/// marked as the front cover (type 3; an MP4 `covr` image counts as one), or
/// the file's only picture whatever its type (FFmpeg marks an attached
/// picture "other" by default). A file with several pictures and not exactly
/// one front cover goes without a cover; [`cover_left_out`] says why. Every
/// picture stays in the transferred file either way.
pub fn cover_index(pictures: &[Picture]) -> Option<usize> {
    if pictures.len() == 1 {
        return Some(0);
    }
    let mut fronts = pictures
        .iter()
        .enumerate()
        .filter(|(_, p)| p.picture_type == 3)
        .map(|(index, _)| index);
    match (fronts.next(), fronts.next()) {
        (Some(index), None) => Some(index),
        _ => None,
    }
}

/// The picture [`cover_index`] chooses.
pub fn cover_picture(pictures: &[Picture]) -> Option<&Picture> {
    cover_index(pictures).map(|index| &pictures[index])
}

/// Why a source that holds pictures goes without a cover; `None` when it
/// has its cover or holds no picture.
pub fn cover_left_out(pictures: &[Picture]) -> Option<String> {
    if pictures.is_empty() || cover_index(pictures).is_some() {
        return None;
    }
    let fronts = pictures.iter().filter(|p| p.picture_type == 3).count();
    Some(if fronts == 0 {
        format!(
            "it holds {} pictures and none is marked as the front cover, so it goes without a cover",
            pictures.len()
        )
    } else {
        format!("it holds {fronts} pictures marked as the front cover, so it goes without a cover")
    })
}

/// Exact cover bytes (the picture [`cover_index`] chooses, as the file holds
/// it) without assigning an MP4 checksum or a native artwork permit. Every
/// other source picture remains embedded and hash-bound in the unchanged
/// transferred file.
pub struct EmbeddedCover {
    pub(crate) bytes: Vec<u8>,
    pub(crate) facts: Picture,
}
impl EmbeddedCover {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn facts(&self) -> &Picture {
        &self.facts
    }
}
pub fn read_front_cover(path: &Path, expected: &Source) -> Result<Option<EmbeddedCover>> {
    let identity = expected.transferred_identity()?;
    let mut held = crate::media::held_source::BoundSource::open_observed(
        path,
        identity.file_bytes,
        identity.full_file_sha256,
    )?;
    let bytes = held.read_for_retention()?;
    if expected.stream.is_some() {
        return crate::media::audio_source::front_cover(&bytes, expected);
    }
    let tag = bytes
        .get(..usize::try_from(expected.audio_offset)?)
        .context("MP3 tag span differs")?;
    let chosen = cover_index(&expected.artwork);
    let (mut front, mut at) = (None, 0);
    let (tags, version, frames, pictures) = parse_id3_with_picture(tag, &mut |facts, image| {
        if Some(at) == chosen {
            front = Some(EmbeddedCover {
                bytes: image.to_vec(),
                facts: facts.clone(),
            });
        }
        at += 1;
        Ok(())
    })?;
    ensure!(
        tags == expected.metadata
            && version == expected.id3_version
            && frames == expected.frames
            && pictures == expected.artwork,
        "MP3 artwork/source inspection differs"
    );
    ensure!(
        chosen.is_none() || front.is_some(),
        "MP3 cover picture is absent"
    );
    Ok(front)
}
