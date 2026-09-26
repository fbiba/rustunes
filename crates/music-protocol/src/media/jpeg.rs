//! Bounded JPEG structure facts; these do not establish entropy-decode or cache ownership.
use anyhow::{Context, Result, bail, ensure};
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// The raster size of a phone rendition: one baseline 8-bit three-component
/// frame and one scan, the only shape the phone writes.
pub fn dimensions(bytes: &[u8]) -> Result<(u16, u16)> {
    walk(bytes, false)
}

/// The raster size of a source cover: any frame type, precision and
/// component count, and any number of scans (a progressive JPEG has several).
/// Apple's sync sends a cover JPEG unchanged whatever its kind, and has sent
/// progressive and grayscale ones.
pub fn source_dimensions(bytes: &[u8]) -> Result<(u16, u16)> {
    walk(bytes, true)
}

fn walk(bytes: &[u8], any_kind: bool) -> Result<(u16, u16)> {
    ensure!(
        bytes.len() >= 4 && bytes.len() as u64 <= MAX_IMAGE_BYTES && bytes[..2] == [0xff, 0xd8],
        "not a bounded JPEG"
    );
    let mut at = 2;
    let mut frame = None;
    let mut scan = false;
    while at < bytes.len() {
        ensure!(bytes[at] == 0xff, "invalid JPEG marker");
        while at < bytes.len() && bytes[at] == 0xff {
            at += 1;
        }
        let marker = *bytes.get(at).context("truncated JPEG marker")?;
        at += 1;
        if marker == 0xd9 {
            ensure!(
                scan && at == bytes.len(),
                "JPEG missing scan or trailing data"
            );
            return frame.context("JPEG frame absent");
        }
        ensure!(
            !matches!(marker, 0x00 | 0xd0..=0xd8),
            "unexpected standalone JPEG marker"
        );
        let raw = bytes
            .get(at..at + 2)
            .context("truncated JPEG segment length")?;
        let length = u16::from_be_bytes(raw.try_into()?) as usize;
        ensure!(length >= 2, "invalid JPEG segment length");
        let segment = bytes
            .get(at + 2..at + length)
            .context("truncated JPEG segment")?;
        at += length;
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            ensure!(frame.is_none(), "repeated JPEG frame");
            if any_kind {
                let components = usize::from(*segment.get(5).context("truncated JPEG frame")?);
                ensure!(
                    components > 0 && segment.len() == 6 + 3 * components,
                    "invalid JPEG frame"
                );
            } else {
                ensure!(
                    marker == 0xc0 && segment.len() == 15 && segment[0] == 8 && segment[5] == 3,
                    "unsupported JPEG frame"
                );
            }
            let height = u16::from_be_bytes(segment[1..3].try_into()?);
            let width = u16::from_be_bytes(segment[3..5].try_into()?);
            ensure!(width > 0 && height > 0, "empty JPEG raster");
            frame = Some((width, height));
        }
        if marker == 0xda {
            ensure!(
                frame.is_some() && (any_kind || !scan),
                "unsupported repeated JPEG scan"
            );
            scan = true;
            loop {
                let byte = *bytes.get(at).context("JPEG lacks end marker")?;
                at += 1;
                if byte != 0xff {
                    continue;
                }
                let start = at - 1;
                while bytes.get(at) == Some(&0xff) {
                    at += 1;
                }
                let next = *bytes.get(at).context("truncated JPEG scan")?;
                if next == 0 || (0xd0..=0xd7).contains(&next) {
                    at += 1;
                    continue;
                }
                at = start;
                break;
            }
        }
    }
    bail!("JPEG lacks end marker")
}
