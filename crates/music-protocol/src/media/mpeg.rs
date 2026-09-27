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
pub(crate) fn number_pair(s: &str) -> Result<(u32, Option<u32>)> {
    let mut p = s.split('/');
    let number = p.next().context("empty numeric tag")?.parse()?;
    let total = p.next().map(str::parse).transpose()?;
    ensure!(p.next().is_none(), "invalid numeric pair");
    Ok((number, total))
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
