//! Bounded PNG structure facts for a source cover, and the JPEG sync sends in
//! its place. Apple's sync sends a PNG cover unchanged, but the phone then
//! keeps its own JPEG of it, never the PNG, so no PNG could be verified.
use anyhow::{Context, Result, ensure};
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// The largest raster converted: a 6,000-pixel square cover.
pub(crate) const MAX_PIXELS: u64 = 36_000_000;
const JPEG_QUALITY: u8 = 95;
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// The raster size of a PNG: its first chunk is the header, every chunk's
/// CRC holds, image data is present and the end chunk closes the file.
pub fn dimensions(bytes: &[u8]) -> Result<(u16, u16)> {
    ensure!(
        bytes.len() as u64 <= MAX_IMAGE_BYTES && bytes.starts_with(&SIGNATURE),
        "not a bounded PNG"
    );
    let mut at = SIGNATURE.len();
    let mut header = None;
    let mut data = false;
    loop {
        let length = u32::from_be_bytes(
            bytes
                .get(at..at + 4)
                .context("truncated PNG chunk length")?
                .try_into()?,
        ) as usize;
        let kind = bytes
            .get(at + 4..at + 8)
            .context("truncated PNG chunk type")?;
        ensure!(
            kind.iter().all(u8::is_ascii_alphabetic),
            "invalid PNG chunk type"
        );
        let body = bytes
            .get(at + 8..at + 8 + length)
            .context("truncated PNG chunk")?;
        let stored = bytes
            .get(at + 8 + length..at + 12 + length)
            .context("truncated PNG chunk CRC")?;
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(body);
        ensure!(crc.sum().to_be_bytes() == stored, "PNG chunk CRC differs");
        at += 12 + length;
        match kind {
            b"IHDR" => {
                ensure!(
                    header.is_none() && at == SIGNATURE.len() + 25 && length == 13,
                    "PNG header must come first, once"
                );
                let width = u32::from_be_bytes(body[0..4].try_into()?);
                let height = u32::from_be_bytes(body[4..8].try_into()?);
                let (depth, color) = (body[8], body[9]);
                ensure!(
                    match color {
                        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                        3 => matches!(depth, 1 | 2 | 4 | 8),
                        2 | 4 | 6 => matches!(depth, 8 | 16),
                        _ => false,
                    } && body[10] == 0
                        && body[11] == 0
                        && body[12] <= 1,
                    "unsupported PNG header"
                );
                ensure!(width > 0 && height > 0, "empty PNG raster");
                header = Some((
                    u16::try_from(width).context("PNG wider than 65535 pixels")?,
                    u16::try_from(height).context("PNG taller than 65535 pixels")?,
                ));
            }
            b"IDAT" => {
                ensure!(header.is_some(), "PNG data before its header");
                data = true;
            }
            b"IEND" => {
                ensure!(
                    data && length == 0 && at == bytes.len(),
                    "PNG missing image data or trailing data"
                );
                return header.context("PNG header absent");
            }
            _ => ensure!(header.is_some(), "PNG chunk before its header"),
        }
    }
}

/// The cover sync sends for a PNG: a baseline JPEG of the same raster, its
/// transparency flattened onto white as the phone does, at quality 95 without
/// chroma subsampling. The phone keeps a JPEG byte for byte, so this is the
/// original the phone holds. The same PNG always gives the same bytes.
pub fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>> {
    let (width, height) = dimensions(bytes)?;
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "PNG cover of {width}x{height} exceeds {MAX_PIXELS} pixels"
    );
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(bytes),
        png::Limits {
            bytes: 8 * MAX_PIXELS as usize,
        },
    );
    // Palettes and transparency chunks become channels; samples become 8-bit.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("decoding PNG cover")?;
    let mut buffer = vec![0; reader.output_buffer_size().context("PNG frame size")?];
    let frame = reader
        .next_frame(&mut buffer)
        .context("decoding PNG cover")?;
    ensure!(
        frame.bit_depth == png::BitDepth::Eight
            && (frame.width, frame.height) == (u32::from(width), u32::from(height)),
        "decoded PNG frame differs from its header"
    );
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => anyhow::bail!("PNG palette was not expanded"),
    };
    let row_bytes = usize::from(width) * channels;
    let mut rgb = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for row in buffer[..frame.buffer_size()].chunks_exact(frame.line_size) {
        for pixel in row[..row_bytes].chunks_exact(channels) {
            let (color, alpha) = match *pixel {
                [gray] => ([gray; 3], 255),
                [gray, alpha] => ([gray; 3], alpha),
                [r, g, b] => ([r, g, b], 255),
                [r, g, b, alpha] => ([r, g, b], alpha),
                _ => unreachable!("chunks have one to four channels"),
            };
            rgb.extend(color.map(|value| over_white(value, alpha)));
        }
    }
    encode(&rgb, width, height)
}

/// A baseline JPEG of an RGB raster, at quality 95 without chroma
/// subsampling: the cover sync sends for any image the phone would not keep.
pub(crate) fn encode(rgb: &[u8], width: u16, height: u16) -> Result<Vec<u8>> {
    let mut jpeg = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, JPEG_QUALITY);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    encoder
        .encode(rgb, width, height, jpeg_encoder::ColorType::Rgb)
        .context("encoding the JPEG of a cover")?;
    ensure!(
        crate::media::jpeg::dimensions(&jpeg)? == (width, height),
        "the JPEG of a cover is not one baseline frame of its raster"
    );
    Ok(jpeg)
}

/// A channel value with its alpha composited over white.
pub(crate) fn over_white(value: u8, alpha: u8) -> u8 {
    let (value, alpha) = (u32::from(value), u32::from(alpha));
    ((value * alpha + 255 * (255 - alpha) + 127) / 255) as u8
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(body);
        [
            &(body.len() as u32).to_be_bytes()[..],
            kind,
            body,
            &crc.sum().to_be_bytes(),
        ]
        .concat()
    }

    /// A well-formed PNG of the given raster, for reader tests.
    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let header = [
            &width.to_be_bytes()[..],
            &height.to_be_bytes(),
            &[8, 2, 0, 0, 0],
        ]
        .concat();
        [
            &SIGNATURE[..],
            &chunk(b"IHDR", &header),
            &chunk(b"tEXt", b"Comment\0x"),
            &chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]),
            &chunk(b"IEND", &[]),
        ]
        .concat()
    }

    /// A real PNG with pixel data, encoded by the png crate.
    fn encoded(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
        out
    }

    #[test]
    fn converts_every_color_type_to_one_baseline_jpeg_the_same_each_time() {
        let rgba = encoded(
            3,
            2,
            png::ColorType::Rgba,
            &[
                255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 9, 9, 255, 200, 100, 50, 255, 0,
                0, 0, 0,
            ],
        );
        let jpeg = to_jpeg(&rgba).unwrap();
        assert_eq!(crate::media::jpeg::dimensions(&jpeg).unwrap(), (3, 2));
        assert_eq!(to_jpeg(&rgba).unwrap(), jpeg);
        for (color, channels) in [
            (png::ColorType::Grayscale, 1),
            (png::ColorType::GrayscaleAlpha, 2),
            (png::ColorType::Rgb, 3),
        ] {
            let image = encoded(4, 4, color, &vec![77; 16 * channels]);
            let jpeg = to_jpeg(&image).unwrap();
            // Always three components, so the phone makes its renditions.
            assert_eq!(crate::media::jpeg::dimensions(&jpeg).unwrap(), (4, 4));
        }
        // Structure alone is not enough: the image data must decode.
        assert!(to_jpeg(&png(4, 4)).is_err());
    }

    #[test]
    fn transparency_is_flattened_onto_white() {
        assert_eq!(over_white(0, 0), 255);
        assert_eq!(over_white(0, 255), 0);
        assert_eq!(over_white(100, 128), 177);
    }

    #[test]
    #[ignore = "converts PNG_COVER_SOURCE into PNG_COVER_OUTPUT for inspection"]
    fn converts_a_png_file() {
        let source = std::fs::read(std::env::var_os("PNG_COVER_SOURCE").unwrap()).unwrap();
        let output = std::env::var_os("PNG_COVER_OUTPUT").unwrap();
        std::fs::write(output, to_jpeg(&source).unwrap()).unwrap();
    }

    #[test]
    fn reads_the_header_raster_of_a_well_formed_png() {
        assert_eq!(dimensions(&png(300, 200)).unwrap(), (300, 200));
    }

    #[test]
    fn refuses_damaged_truncated_and_unbounded_pngs() {
        let good = png(4, 4);
        assert!(dimensions(&good[..good.len() - 1]).is_err());
        let mut bad = good.clone();
        bad[20] ^= 1; // A header byte the CRC covers.
        assert!(dimensions(&bad).is_err());
        let trailing = [&good[..], b"x"].concat();
        assert!(dimensions(&trailing).is_err());
        assert!(dimensions(&png(70_000, 4)).is_err());
        assert!(dimensions(&png(0, 4)).is_err());
        // Image data is required before the end.
        let empty = [&good[..33], &chunk(b"IEND", &[])].concat();
        assert!(dimensions(&empty).is_err());
    }
}
