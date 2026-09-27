//! Source inspection for every format other than ALAC and MPEG-1 Layer III:
//! AAC, HE-AAC and xHE-AAC in MP4, FLAC, WAV, AIFF and MPEG-2/2.5 Layer III.
//! The container reader gives the encoded identity and stream facts; tags and
//! pictures come from the container's own tag store (MP4 `ilst`, Vorbis
//! comments and FLAC pictures, ID3v2 in WAV, AIFF or MPEG, RIFF INFO, AIFF
//! text chunks). Nothing is decoded, rewritten or granted.
use std::io::{Cursor, Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::media::{
    container::{self, Layout, TagKind, read_span},
    fingerprint::EncodedAudio,
    format::Format,
    metadata::tag_value,
    mpeg::{
        Comment, EmbeddedCover, EncodedMpeg, Fingerprint, FrameEvidence, Picture, Source, Tags, hex,
    },
};

const MAX_TEXT: usize = 64 * 1024;

/// What a source of these formats carries in place of MPEG-1 frame timing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Stream {
    pub format: Format,
    /// PCM bits of a lossless format; 0 for a lossy one.
    pub bits_per_sample: u8,
    pub bit_rate_kbps: u32,
    pub variable_bit_rate: bool,
    pub encoder_delay: u64,
    pub encoder_padding: u64,
    /// Samples a player plays: the coded ones less delay and padding.
    pub valid_samples: u64,
    #[serde(skip)]
    pub encoded: EncodedAudio,
}

/// The source a container reader's layout describes: identity, stream, tags
/// and pictures.
pub(crate) fn read_layout(
    file: &mut (impl Read + Seek),
    size: u64,
    layout: Layout,
) -> Result<Source> {
    let e = &layout.encoded;
    e.validate(size)?;
    let full = container::hash_span(file, 0, size)?;
    let (metadata, id3_version, frames) = read_tags(file, &layout)?;
    let mut artwork = Vec::new();
    read_pictures(file, &layout, &mut |facts, _| {
        artwork.push(facts);
        Ok(())
    })?;
    ensure!(artwork.len() <= 8, "too many embedded pictures");
    let stream = Stream {
        format: e.format,
        bits_per_sample: e.bits_per_sample,
        bit_rate_kbps: layout.stream.bit_rate_kbps,
        variable_bit_rate: layout.stream.variable_bit_rate,
        encoder_delay: layout.stream.encoder_delay,
        encoder_padding: layout.stream.encoder_padding,
        valid_samples: layout.stream.valid_samples(e.coded_samples)?,
        encoded: e.clone(),
    };
    let apple_timing = match e.format {
        Format::Mpeg2Layer3 | Format::Mpeg25Layer3 => Some(
            crate::media::mpeg::gapless::analyse(
                file,
                e.audio_offset,
                e.audio_bytes,
                &metadata.encoder_hints,
            )
            .context("Apple's MPEG timing")?,
        ),
        _ => None,
    };
    let source = Source {
        codec: e.format.name(),
        fingerprint: Fingerprint {
            file_bytes: size,
            full_file_sha256: hex(&full),
            audio: EncodedMpeg {
                format: "encoded-audio",
                sha256: hex(&e.sha256()),
                frame_bytes_sha256: hex(&e.audio_sha256),
                sample_rate: e.sample_rate,
                channels: e.channels,
                frame_count: u32::try_from(e.packets).context("too many audio packets")?,
                audio_bytes: e.audio_bytes,
                coded_samples: e.coded_samples,
            },
        },
        timing: None,
        stream: Some(stream),
        apple_timing,
        metadata,
        id3_version,
        frames,
        artwork,
        audio_offset: e.audio_offset,
        trailing_tag_bytes: layout
            .tags
            .iter()
            .filter(|t| t.kind == TagKind::Id3v1)
            .map(|t| t.bytes as u32)
            .sum(),
    };
    source.transferred_identity()?;
    Ok(source)
}

/// The cover of `expected` (`mpeg::cover_index`), from `bytes`, the held
/// file's content.
/// The file must still read as `expected`.
pub(crate) fn front_cover(bytes: &[u8], expected: &Source) -> Result<Option<EmbeddedCover>> {
    let size = bytes.len() as u64;
    let mut file = Cursor::new(bytes);
    let layout = container::inspect(&mut file, size)?
        .context("the source is no longer a supported format")?;
    ensure!(
        read_layout(&mut file, size, layout.clone())? == *expected,
        "the source's inspection differs"
    );
    let chosen = crate::media::mpeg::cover_index(&expected.artwork);
    let (mut front, mut at) = (None, 0);
    read_pictures(&mut file, &layout, &mut |facts, image| {
        if Some(at) == chosen {
            front = Some(EmbeddedCover {
                bytes: image.to_vec(),
                facts,
            });
        }
        at += 1;
        Ok(())
    })?;
    ensure!(
        chosen.is_none() || front.is_some(),
        "the source's cover picture is absent"
    );
    Ok(front)
}

/// Tags from the first tag store the container holds that carries any. A
/// file with tags in two stores (an ID3 chunk and RIFF INFO) takes ID3.
fn read_tags(
    file: &mut (impl Read + Seek),
    layout: &Layout,
) -> Result<(Tags, Option<u8>, Vec<FrameEvidence>)> {
    let mut tags = Tags::default();
    let mut version = None;
    let mut fields = Vec::new();
    let regions = &layout.tags;
    if regions.iter().any(|r| r.kind == TagKind::Mp4Movie) {
        file.seek(SeekFrom::Start(0))?;
        let (tags, fields) = crate::media::metadata::mp4_tags(file)?;
        return Ok((tags, None, fields));
    }
    let id3: Vec<_> = regions
        .iter()
        .filter(|r| r.kind == TagKind::Id3v2)
        .collect();
    ensure!(id3.len() <= 1, "more than one ID3v2 tag");
    if let Some(region) = id3.first() {
        let bytes = id3_bytes(file, region.offset, region.bytes)?;
        let parsed;
        (parsed, version, fields, _) = crate::media::mpeg::parse_id3(&bytes)?;
        tags = parsed;
    }
    for region in regions {
        match region.kind {
            TagKind::VorbisComment => {
                let bytes = read_span(file, region.offset, region.bytes)?;
                vorbis_comments(&bytes, &mut tags, &mut fields)?;
            }
            TagKind::RiffInfo if id3.is_empty() => {
                let bytes = read_span(file, region.offset, region.bytes)?;
                riff_info(&bytes, &mut tags, &mut fields)?;
            }
            TagKind::AiffText if id3.is_empty() => {
                let id = read_span(file, region.offset - 8, 4)?;
                let bytes = read_span(file, region.offset, region.bytes)?;
                let id = String::from_utf8_lossy(&id).into_owned();
                let text = latin1_or_utf8(&bytes);
                fields.push(evidence(&id, &bytes));
                match id.as_str() {
                    "NAME" => tags.title.push(text),
                    "AUTH" => tags.artist.push(text),
                    "(c) " => tags.copyright.push(text),
                    _ => {
                        if !text.is_empty() {
                            tags.comments.push(Comment {
                                language: String::new(),
                                description: String::new(),
                                text,
                            })
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok((tags, version, fields))
}

/// The ID3v2 tag in a chunk, without the chunk's padding.
fn id3_bytes(file: &mut (impl Read + Seek), offset: u64, bytes: u64) -> Result<Vec<u8>> {
    let mut data = read_span(file, offset, bytes)?;
    ensure!(
        data.len() >= 10 && &data[..3] == b"ID3",
        "invalid ID3 chunk"
    );
    let size = data[6..10]
        .iter()
        .fold(0usize, |n, b| (n << 7) | usize::from(*b & 0x7f));
    let footer = if data[3] == 4 && data[5] & 0x10 != 0 {
        10
    } else {
        0
    };
    let end = 10 + size + footer;
    ensure!(end <= data.len(), "ID3 tag exceeds its chunk");
    ensure!(
        data[end..].iter().all(|b| *b == 0),
        "data after the ID3 tag in its chunk"
    );
    data.truncate(end);
    Ok(data)
}

/// Every picture the container holds, with its bytes.
fn read_pictures(
    file: &mut (impl Read + Seek),
    layout: &Layout,
    on_picture: &mut impl FnMut(Picture, &[u8]) -> Result<()>,
) -> Result<()> {
    for region in &layout.tags {
        match region.kind {
            TagKind::Mp4Movie => {
                let size = layout_size(file)?;
                for cover in crate::media::artwork::extract_from(file, size)? {
                    let (mime, (width, height)) =
                        crate::media::mpeg::image_facts(cover.mime().as_bytes(), &cover.bytes)?;
                    on_picture(
                        Picture {
                            mime,
                            declared_type: true,
                            picture_type: 3,
                            description: String::new(),
                            bytes: cover.bytes.len(),
                            sha256: hex(&cover.sha256),
                            width,
                            height,
                            structure_verified: true,
                            image_decode_verified: false,
                        },
                        &cover.bytes,
                    )?;
                }
            }
            TagKind::Id3v2 => {
                let bytes = id3_bytes(file, region.offset, region.bytes)?;
                crate::media::mpeg::parse_id3_with_picture(&bytes, &mut |facts, image| {
                    on_picture(facts.clone(), image)
                })?;
            }
            TagKind::FlacPicture => {
                let bytes = read_span(file, region.offset, region.bytes)?;
                let (facts, image) = flac_picture(&bytes)?;
                on_picture(facts, image)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn layout_size(file: &mut (impl Read + Seek)) -> Result<u64> {
    Ok(file.seek(SeekFrom::End(0))?)
}

fn flac_picture(bytes: &[u8]) -> Result<(Picture, &[u8])> {
    let mut at = 0usize;
    let mut word = || -> Result<u32> {
        let value = bytes.get(at..at + 4).context("truncated FLAC picture")?;
        at += 4;
        Ok(u32::from_be_bytes(value.try_into()?))
    };
    let kind = word()?;
    let mime_len = word()? as usize;
    let mime = bytes
        .get(at..at + mime_len)
        .context("truncated FLAC picture type")?
        .to_vec();
    at += mime_len;
    let mut word = || -> Result<u32> {
        let value = bytes.get(at..at + 4).context("truncated FLAC picture")?;
        at += 4;
        Ok(u32::from_be_bytes(value.try_into()?))
    };
    let description_len = word()? as usize;
    let description = std::str::from_utf8(
        bytes
            .get(at..at + description_len)
            .context("truncated FLAC picture description")?,
    )?
    .to_owned();
    at += description_len;
    let mut word = || -> Result<u32> {
        let value = bytes.get(at..at + 4).context("truncated FLAC picture")?;
        at += 4;
        Ok(u32::from_be_bytes(value.try_into()?))
    };
    let (_, _, _, _) = (word()?, word()?, word()?, word()?);
    let length = word()? as usize;
    let image = bytes
        .get(at..at + length)
        .context("truncated FLAC picture data")?;
    ensure!(at + length == bytes.len(), "data after the FLAC picture");
    ensure!(kind <= 20, "invalid FLAC picture type");
    let (held, (width, height)) = crate::media::mpeg::image_facts(&mime, image)?;
    Ok((
        Picture {
            mime: held,
            declared_type: crate::media::mpeg::declares_type(&mime),
            picture_type: u8::try_from(kind)?,
            description,
            bytes: image.len(),
            sha256: hex(&Sha256::digest(image)),
            width,
            height,
            structure_verified: true,
            image_decode_verified: false,
        },
        image,
    ))
}

/// Vorbis comments: vendor, then `KEY=value` items. Keys are ASCII and match
/// without regard to case.
fn vorbis_comments(bytes: &[u8], tags: &mut Tags, fields: &mut Vec<FrameEvidence>) -> Result<()> {
    let mut at = 0usize;
    let mut take = |count: usize| -> Result<&[u8]> {
        let out = bytes
            .get(at..at + count)
            .context("truncated Vorbis comment")?;
        at += count;
        Ok(out)
    };
    let vendor = u32::from_le_bytes(take(4)?.try_into()?) as usize;
    take(vendor)?;
    let count = u32::from_le_bytes(take(4)?.try_into()?);
    ensure!(count <= 4096, "too many Vorbis comments");
    let mut total = 0usize;
    for _ in 0..count {
        let length = u32::from_le_bytes(take(4)?.try_into()?) as usize;
        let item = take(length)?;
        let text = std::str::from_utf8(item).context("Vorbis comment is not UTF-8")?;
        let (key, value) = text.split_once('=').context("Vorbis comment without '='")?;
        let key = key.to_ascii_uppercase();
        total += value.len();
        ensure!(total <= MAX_TEXT * 4, "Vorbis comments exceed bound");
        fields.push(evidence(&key, item));
        let value = value.to_owned();
        match key.as_str() {
            "TITLE" => tags.title.push(value),
            "ARTIST" => tags.artist.push(value),
            "ALBUM" => tags.album.push(value),
            "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => tags.album_artist.push(value),
            "GENRE" => tags.genre.push(value),
            "COMPOSER" => tags.composer.push(value),
            "COPYRIGHT" => tags.copyright.push(value),
            "DATE" | "YEAR" => tags.release_date.push(value),
            "TITLESORT" => tags.sort_title.push(value),
            "ARTISTSORT" => tags.sort_artist.push(value),
            "ALBUMSORT" => tags.sort_album.push(value),
            "ALBUMARTISTSORT" => tags.sort_album_artist.push(value),
            "COMPOSERSORT" => tags.sort_composer.push(value),
            // An empty number or flag is none, as an empty text is no text.
            "TRACKNUMBER" | "TRACKTOTAL" | "TOTALTRACKS" | "DISCNUMBER" | "DISCTOTAL"
            | "TOTALDISCS" | "COMPILATION"
                if tag_value(&value).is_none() => {}
            "TRACKNUMBER" => {
                let (number, total) = crate::media::mpeg::number_pair(value.trim())?;
                set_once(&mut tags.track, number, "track number")?;
                if let Some(total) = total {
                    set_once(&mut tags.track_total, total, "track total")?;
                }
            }
            "TRACKTOTAL" | "TOTALTRACKS" => {
                set_once(&mut tags.track_total, value.trim().parse()?, "track total")?
            }
            "DISCNUMBER" => {
                let (number, total) = crate::media::mpeg::number_pair(value.trim())?;
                set_once(&mut tags.disc, number, "disc number")?;
                if let Some(total) = total {
                    set_once(&mut tags.disc_total, total, "disc total")?;
                }
            }
            "DISCTOTAL" | "TOTALDISCS" => {
                set_once(&mut tags.disc_total, value.trim().parse()?, "disc total")?
            }
            "COMPILATION" => {
                ensure!(tags.compilation.is_none(), "more than one compilation flag");
                tags.compilation = Some(match value.trim() {
                    "0" => false,
                    "1" => true,
                    _ => bail!("invalid compilation flag"),
                });
            }
            "COMMENT" | "DESCRIPTION" if !value.is_empty() => tags.comments.push(Comment {
                language: String::new(),
                description: String::new(),
                text: value,
            }),
            _ => {}
        }
    }
    ensure!(at == bytes.len(), "data after the Vorbis comments");
    Ok(())
}

/// RIFF `LIST`/`INFO` items: four-letter ids, NUL-terminated text.
fn riff_info(bytes: &[u8], tags: &mut Tags, fields: &mut Vec<FrameEvidence>) -> Result<()> {
    let mut at = 4;
    while at < bytes.len() {
        let header = bytes.get(at..at + 8).context("truncated RIFF INFO item")?;
        let id = String::from_utf8_lossy(&header[..4]).into_owned();
        let length = u32::from_le_bytes(header[4..].try_into()?) as usize;
        let data = bytes
            .get(at + 8..at + 8 + length)
            .context("RIFF INFO item exceeds its list")?;
        at += 8 + length + (length & 1);
        fields.push(evidence(&id, data));
        let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
        let text = latin1_or_utf8(&data[..end]);
        match id.as_str() {
            "INAM" => tags.title.push(text),
            "IART" => tags.artist.push(text),
            "IPRD" => tags.album.push(text),
            "IGNR" => tags.genre.push(text),
            "ICOP" => tags.copyright.push(text),
            "ICRD" => tags.release_date.push(text),
            // An empty track number is none, as an empty text is no text.
            "ITRK" | "IPRT" if tag_value(&text).is_none() => {}
            "ITRK" | "IPRT" => {
                let (number, total) = crate::media::mpeg::number_pair(text.trim())?;
                set_once(&mut tags.track, number, "track number")?;
                if let Some(total) = total {
                    set_once(&mut tags.track_total, total, "track total")?;
                }
            }
            "ICMT" if !text.is_empty() => tags.comments.push(Comment {
                language: String::new(),
                description: String::new(),
                text,
            }),
            _ => {}
        }
    }
    Ok(())
}

fn set_once(slot: &mut Option<u32>, value: u32, field: &str) -> Result<()> {
    ensure!(
        slot.is_none_or(|old| old == value),
        "conflicting {field} values"
    );
    *slot = Some(value);
    Ok(())
}

fn latin1_or_utf8(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|b| char::from(*b)).collect(),
    }
}

fn evidence(id: &str, bytes: &[u8]) -> FrameEvidence {
    FrameEvidence {
        id: id.to_owned(),
        bytes: bytes.len(),
        sha256: hex(&Sha256::digest(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }
}
