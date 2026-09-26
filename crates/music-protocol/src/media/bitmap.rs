//! GIF, BMP and TIFF pictures. Apple's sync converts such a cover to PNG, and
//! the phone keeps no PNG either (it makes its own JPEG of one), so sync sends
//! each as a baseline JPEG of its first frame, made as a PNG's is
//! ([`png::to_jpeg`]).
//!
//! [`png::to_jpeg`]: crate::media::png::to_jpeg
use anyhow::{Context, Result, ensure};
use image::{ImageFormat, ImageReader, Limits};
use std::io::Cursor;

use crate::media::png::{MAX_PIXELS, encode, over_white};

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// A picture format read only to be converted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Gif,
    Bmp,
    Tiff,
}
impl Format {
    /// The format whose signature `bytes` starts with.
    pub fn of(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.starts_with(b"BM") {
            Some(Self::Bmp)
        } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
            Some(Self::Tiff)
        } else {
            None
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
            Self::Tiff => "image/tiff",
        }
    }
    /// The lowercase name reports use, as "png" for a PNG.
    pub fn name(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
        }
    }
    fn image_format(self) -> ImageFormat {
        match self {
            Self::Gif => ImageFormat::Gif,
            Self::Bmp => ImageFormat::Bmp,
            Self::Tiff => ImageFormat::Tiff,
        }
    }
}

/// A decoder bounded to a cover's raster: at most 65,535 pixels a side and
/// the RGBA buffer of `MAX_PIXELS`.
fn reader(bytes: &[u8], format: Format) -> ImageReader<Cursor<&[u8]>> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.image_format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(u32::from(u16::MAX));
    limits.max_image_height = Some(u32::from(u16::MAX));
    limits.max_alloc = Some(4 * MAX_PIXELS);
    reader.limits(limits);
    reader
}

/// The raster size `format`'s header gives; `bytes` must carry its signature.
pub fn dimensions(bytes: &[u8], format: Format) -> Result<(u16, u16)> {
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES && Format::of(bytes) == Some(format),
        "not a bounded {} picture",
        format.name().to_uppercase()
    );
    let (width, height) = reader(bytes, format)
        .into_dimensions()
        .with_context(|| format!("reading a {} header", format.name().to_uppercase()))?;
    ensure!(width > 0 && height > 0, "empty picture raster");
    Ok((
        u16::try_from(width).context("picture wider than 65535 pixels")?,
        u16::try_from(height).context("picture taller than 65535 pixels")?,
    ))
}

/// The cover sync sends for a GIF, BMP or TIFF: a baseline JPEG of its first
/// frame, transparency flattened onto white, as for a PNG. The same picture
/// always gives the same bytes.
pub fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>> {
    let format = Format::of(bytes).context("not a GIF, BMP or TIFF picture")?;
    let name = format.name().to_uppercase();
    let (width, height) = dimensions(bytes, format)?;
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "{name} cover of {width}x{height} exceeds {MAX_PIXELS} pixels"
    );
    let image = reader(bytes, format)
        .decode()
        .with_context(|| format!("decoding a {name} cover"))?
        .to_rgba8();
    ensure!(
        image.dimensions() == (u32::from(width), u32::from(height)),
        "decoded {name} frame differs from its header"
    );
    let rgb: Vec<u8> = image
        .as_raw()
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]].map(|value| over_white(value, p[3])))
        .collect();
    encode(&rgb, width, height)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
}
