//! Read the source's embedded cover without rewriting the file. The
//! supported native MP4 checksum is count + full `data` atom length, not an
//! image digest. Each `covr` image is a JPEG (type 13), PNG (14), GIF (12) or
//! BMP (27) data atom; the file's only one is its cover, and anything but a
//! JPEG is sent as a JPEG ([`sent_cover`]). A file with several goes without
//! a cover, as `mpeg::cover_index` decides for other formats.

use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

const MAX_SOURCE: u64 = 512 * 1024 * 1024;
const MAX_COVER: u64 = 32 * 1024 * 1024;
const MAX_BOXES: usize = 4096;

#[derive(Clone)]
pub struct SourceArtwork {
    pub source_size: u64,
    pub source_sha256: [u8; 32],
    /// The cover sync sends.
    pub cover: Option<Cover>,
    /// The PNG, GIF, BMP or TIFF the file holds when `cover` is its JPEG.
    pub embedded: Option<EmbeddedImage>,
    /// Why a file that holds cover images goes without a cover.
    pub left_out: Option<String>,
}

/// An embedded image sync converts: its format, identity and raster.
#[derive(Clone)]
pub struct EmbeddedImage {
    /// "png", "gif", "bmp" or "tiff".
    pub format: &'static str,
    pub sha256: [u8; 32],
    pub width: u16,
    pub height: u16,
}

#[derive(Clone)]
pub struct Cover {
    pub bytes: Vec<u8>,
    pub sha256: [u8; 32],
    pub checksum: u32,
}
impl Cover {
    /// The image's type by its own bytes: PNG, GIF, BMP, TIFF or JPEG.
    pub fn mime(&self) -> &'static str {
        if self.bytes.starts_with(&crate::media::png::SIGNATURE) {
            "image/png"
        } else {
            crate::media::bitmap::Format::of(&self.bytes)
                .map_or("image/jpeg", crate::media::bitmap::Format::mime)
        }
    }
    /// The image's raster and format name when sync sends it converted.
    fn converted(&self) -> Result<Option<EmbeddedImage>> {
        use crate::media::bitmap::Format;
        let (format, (width, height)) = if self.mime() == "image/png" {
            ("png", crate::media::png::dimensions(&self.bytes)?)
        } else if let Some(format) = Format::of(&self.bytes) {
            (
                format.name(),
                crate::media::bitmap::dimensions(&self.bytes, format)?,
            )
        } else {
            return Ok(None);
        };
        Ok(Some(EmbeddedImage {
            format,
            sha256: self.sha256,
            width,
            height,
        }))
    }
}

pub fn read(path: &Path) -> Result<SourceArtwork> {
    let path_before = path
        .symlink_metadata()
        .context("reading artwork source attributes")?;
    ensure!(
        path_before.file_type().is_file(),
        "artwork source must be a regular nonsymlink file"
    );
    let file = File::open(path).context("opening artwork source read-only")?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "artwork source must be a regular file");
    ensure!(
        same_file(&path_before, &before),
        "artwork source changed before open"
    );
    ensure!(
        before.len() > 0 && before.len() <= MAX_SOURCE,
        "artwork source exceeds the bounded MP4 size"
    );
    let mut source = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut remaining = before.len();
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        source.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let images = extract(&mut source, before.len())?;
    let (cover, embedded, left_out) = match images.as_slice() {
        [] => (None, None, None),
        [image] => (
            Some(sent_cover(&image.bytes, MP4_CHECKSUM_HEADER)?),
            image.converted()?,
            None,
        ),
        several => (
            None,
            None,
            Some(format!(
                "it holds {} cover images, so it goes without a cover",
                several.len()
            )),
        ),
    };
    let after = source.get_ref().metadata()?;
    ensure!(
        same_file(&before, &after) && same_file(&before, &path.symlink_metadata()?),
        "source changed while reading artwork"
    );
    Ok(SourceArtwork {
        source_size: before.len(),
        source_sha256: digest.finalize().into(),
        cover,
        embedded,
        left_out,
    })
}

/// What an MP4 cover's checksum adds to its image length: the 16-byte `data`
/// atom header and the picture count.
pub const MP4_CHECKSUM_HEADER: u32 = 17;

/// The cover sync sends for an embedded image: a JPEG as it is, a PNG, GIF,
/// BMP or TIFF as a JPEG ([`png::to_jpeg`], [`bitmap::to_jpeg`]), because the
/// phone keeps only a JPEG. The checksum is Apple's for a file holding that
/// image: its length plus `header` (17 for an MP4 cover, 1 for an ID3 or FLAC
/// picture).
///
/// [`png::to_jpeg`]: crate::media::png::to_jpeg
/// [`bitmap::to_jpeg`]: crate::media::bitmap::to_jpeg
pub fn sent_cover(image: &[u8], header: u32) -> Result<Cover> {
    let bytes = if image.starts_with(&crate::media::png::SIGNATURE) {
        crate::media::png::to_jpeg(image)?
    } else if crate::media::bitmap::Format::of(image).is_some() {
        crate::media::bitmap::to_jpeg(image)?
    } else {
        image.to_vec()
    };
    Ok(Cover {
        sha256: Sha256::digest(&bytes).into(),
        checksum: u32::try_from(bytes.len())?
            .checked_add(header)
            .context("artwork checksum overflow")?,
        bytes,
    })
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    right.file_type().is_file()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[derive(Clone, Copy)]
struct Atom {
    kind: [u8; 4],
    declared_size: u32,
    start: u64,
    data: u64,
    end: u64,
}

/// The cover images of an MP4 file already held open, without the
/// whole-file checks [`read`] makes.
pub(crate) fn extract_from(source: &mut (impl Read + Seek), size: u64) -> Result<Vec<Cover>> {
    extract(source, size)
}

/// Every `covr` image, in file order, as the file holds it.
fn extract(source: &mut (impl Read + Seek), size: u64) -> Result<Vec<Cover>> {
    let mut budget = MAX_BOXES;
    let top = boxes(source, 0, size, &mut budget)?;
    let moov = unique(&top, b"moov")?.context("missing MP4 movie")?;
    let movie = boxes(source, moov.data, moov.end, &mut budget)?;
    let direct_meta = unique(&movie, b"meta")?;
    let user_meta = match unique(&movie, b"udta")? {
        Some(udta) => unique(&boxes(source, udta.data, udta.end, &mut budget)?, b"meta")?,
        None => None,
    };
    ensure!(
        direct_meta.is_none() || user_meta.is_none(),
        "multiple MP4 metadata containers"
    );
    let Some(meta) = direct_meta.or(user_meta) else {
        return Ok(Vec::new());
    };
    ensure!(meta.end - meta.data >= 4, "truncated meta flags");
    source.seek(SeekFrom::Start(meta.data))?;
    let mut flags = [0; 4];
    source.read_exact(&mut flags)?;
    ensure!(flags == [0; 4], "unsupported MP4 metadata flags");
    let metadata = boxes(source, meta.data + 4, meta.end, &mut budget)?;
    let Some(ilst) = unique(&metadata, b"ilst")? else {
        return Ok(Vec::new());
    };
    let tags = boxes(source, ilst.data, ilst.end, &mut budget)?;
    let Some(covr) = unique(&tags, b"covr")? else {
        return Ok(Vec::new());
    };
    let values = boxes(source, covr.data, covr.end, &mut budget)?;
    ensure!(
        !values.is_empty() && values.iter().all(|value| value.kind == *b"data"),
        "artwork requires JPEG, PNG, GIF or BMP data atoms"
    );
    values.into_iter().map(|data| image(source, data)).collect()
}

/// One `covr` data atom's image, checked against its declared type.
fn image(source: &mut (impl Read + Seek), data: Atom) -> Result<Cover> {
    use crate::media::bitmap::Format;
    ensure!(
        data.declared_size >= 20 && data.data - data.start == 8 && data.end - data.data >= 12,
        "unsupported artwork atom header or truncated image"
    );
    let length = data.end - data.data - 8;
    ensure!(length <= MAX_COVER, "embedded artwork exceeds size limit");
    source.seek(SeekFrom::Start(data.data))?;
    let mut header = [0u8; 8];
    source.read_exact(&mut header)?;
    ensure!(
        matches!(header, [0, 0, 0, 12 | 13 | 14 | 27, 0, 0, 0, 0]),
        "artwork requires JPEG (type 13), PNG (14), GIF (12) or BMP (27) with locale zero"
    );
    let mut bytes = vec![0; usize::try_from(length)?];
    source.read_exact(&mut bytes)?;
    match header[3] {
        13 => ensure!(
            bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]),
            "embedded JPEG markers are invalid"
        ),
        14 => {
            crate::media::png::dimensions(&bytes).context("embedded PNG is invalid")?;
        }
        kind => {
            let format = if kind == 12 { Format::Gif } else { Format::Bmp };
            crate::media::bitmap::dimensions(&bytes, format)
                .with_context(|| format!("embedded {} is invalid", format.name().to_uppercase()))?;
        }
    }
    let checksum = u32::try_from(data.end - data.start)?
        .checked_add(1)
        .context("artwork checksum overflow")?;
    Ok(Cover {
        sha256: Sha256::digest(&bytes).into(),
        bytes,
        checksum,
    })
}

fn unique(atoms: &[Atom], kind: &[u8; 4]) -> Result<Option<Atom>> {
    let mut matches = atoms.iter().filter(|atom| atom.kind == *kind);
    let first = matches.next().copied();
    ensure!(matches.next().is_none(), "duplicate artwork-path container");
    Ok(first)
}

fn boxes(
    source: &mut (impl Read + Seek),
    start: u64,
    end: u64,
    budget: &mut usize,
) -> Result<Vec<Atom>> {
    ensure!(start <= end, "invalid MP4 container range");
    let mut position = start;
    let mut result = Vec::new();
    while position < end {
        ensure!(*budget > 0, "MP4 artwork box limit exceeded");
        *budget -= 1;
        ensure!(end - position >= 8, "truncated MP4 atom header");
        source.seek(SeekFrom::Start(position))?;
        let mut header = [0u8; 8];
        source.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, header_size) = match short_size {
            0 => (end - position, 8),
            1 => {
                ensure!(end - position >= 16, "truncated extended MP4 atom");
                let mut extended = [0u8; 8];
                source.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        ensure!(
            size >= header_size && size <= end - position,
            "MP4 atom extends outside its container"
        );
        result.push(Atom {
            kind: header[4..].try_into().unwrap(),
            declared_size: short_size,
            start: position,
            data: position + header_size,
            end: position + size,
        });
        position += size;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut value = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        value.extend_from_slice(kind);
        value.extend_from_slice(payload);
        value
    }

    fn source(values: &[u8]) -> Vec<u8> {
        let ilst = atom(b"ilst", &atom(b"covr", values));
        let mut meta = vec![0; 4];
        meta.extend(ilst);
        atom(b"moov", &atom(b"udta", &atom(b"meta", &meta)))
    }

    fn jpeg() -> Vec<u8> {
        atom(b"data", &[0, 0, 0, 13, 0, 0, 0, 0, 0xff, 0xd8, 0xff, 0xd9])
    }

    fn parse(bytes: &[u8]) -> Result<Vec<Cover>> {
        extract(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    /// The one image of an MP4 file.
    fn one(bytes: &[u8]) -> Result<Cover> {
        let mut images = parse(bytes)?;
        ensure!(images.len() == 1, "{} images", images.len());
        Ok(images.remove(0))
    }

    #[test]
    fn preserves_image_bytes_and_counts_data_header_in_checksum() {
        let cover = one(&source(&jpeg())).unwrap();
        assert_eq!(cover.bytes, [0xff, 0xd8, 0xff, 0xd9]);
        assert_eq!(cover.checksum, 21); // 4 JPEG bytes + 16 data header + 1 count.
        assert_eq!(cover.sha256, <[u8; 32]>::from(Sha256::digest(&cover.bytes)));
    }

    #[test]
    fn takes_a_png_cover_with_the_same_checksum_rule() {
        let image = crate::media::png::tests::png(6, 6);
        let data = atom(b"data", &[&[0, 0, 0, 14, 0, 0, 0, 0][..], &image].concat());
        let cover = one(&source(&data)).unwrap();
        assert_eq!(
            (cover.bytes.as_slice(), cover.mime()),
            (image.as_slice(), "image/png")
        );
        assert_eq!(cover.checksum as usize, image.len() + 17);
        let mut damaged = data;
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(parse(&source(&damaged)).is_err());
    }

    #[test]
    fn ignores_cover_like_bytes_in_media_and_reports_absence() {
        let mut bytes = atom(b"mdat", &source(&jpeg()));
        bytes.extend(atom(b"moov", &[]));
        assert!(parse(&bytes).unwrap().is_empty());
    }

    #[test]
    fn reads_every_image_and_sends_a_gif_or_bmp_as_its_jpeg() {
        let mut two = jpeg();
        two.extend(jpeg());
        assert_eq!(parse(&source(&two)).unwrap().len(), 2);
        use crate::media::bitmap::{Format, tests::picture};
        for (kind, format) in [(12, Format::Gif), (27, Format::Bmp)] {
            let image = picture(format);
            let data = atom(
                b"data",
                &[&[0, 0, 0, kind, 0, 0, 0, 0][..], &image].concat(),
            );
            let cover = one(&source(&data)).unwrap();
            assert_eq!(cover.mime(), format.mime());
            let converted = cover.converted().unwrap().unwrap();
            assert_eq!((converted.format, converted.width), (format.name(), 2));
            let sent = sent_cover(&cover.bytes, MP4_CHECKSUM_HEADER).unwrap();
            assert_eq!(sent.mime(), "image/jpeg");
            assert_eq!(sent.checksum as usize, sent.bytes.len() + 17);
        }
    }

    #[test]
    fn rejects_unsupported_embedded_artwork() {
        // A PNG, GIF or BMP type holding JPEG bytes, and a type that is none.
        for kind in [12, 14, 27] {
            let mut declared = jpeg();
            declared[11] = kind;
            assert!(parse(&source(&declared)).is_err());
        }
        let mut other = jpeg();
        other[11] = 1;
        assert!(parse(&source(&other)).is_err());
        let mut locale = jpeg();
        locale[15] = 1;
        assert!(parse(&source(&locale)).is_err());
        let mut malformed = jpeg();
        malformed[19] = 0;
        assert!(parse(&source(&malformed)).is_err());
    }

    #[test]
    fn rejects_atom_boundary_escape_truncation_and_duplicate_containers() {
        let mut invalid = jpeg();
        invalid[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&source(&invalid)).is_err());
        let mut bytes = source(&jpeg());
        bytes.pop();
        assert!(parse(&bytes).is_err());
        let mut double = source(&jpeg());
        double.extend(source(&jpeg()));
        assert!(parse(&double).is_err());
    }

    #[test]
    fn rejects_zero_sized_data_atom_without_a_native_checksum_contract() {
        let mut data = jpeg();
        data[..4].copy_from_slice(&0u32.to_be_bytes());
        assert!(parse(&source(&data)).is_err());
    }
}
