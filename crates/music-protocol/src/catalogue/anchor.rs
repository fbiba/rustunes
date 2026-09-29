//! Read-only saved host-anchor inspection for the current iTunesDB/CDB format.
//!
//! The value is saved host state, not a fresh device Media anchor. This module
//! checks the container and dataset 11 layout; it does not authenticate the
//! database, validate track records, or establish that a sync completed.
//!
//! Format evidence: pinned AMPDevicesAgent.exe SHA-256
//! f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9,
//! RVAs 0x47b060 (root writer), 0x483850 (root reader), 0x483468 (zlib),
//! 0x475a64 / 0x48142c (dataset 11), 0x487620 / 0x46b2ec (Media first).

use anyhow::{Context, Result, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};

pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SECTION_BYTES: usize = 256 * 1024 * 1024;
const MAX_SECTIONS: u32 = 4096;
const ROOT_HEADER_BYTES: usize = 0xf4;
const ANCHOR_HEADER_BYTES: usize = 0x60;
const ANCHOR_PAYLOAD_BYTES: usize = 0x30;

pub struct AnchorState {
    saved_media_anchor: u32,
    saved_voice_memo_anchor: u32,
    database_pid: u64,
}

impl AnchorState {
    /// The dataset 11 Media hostLast value saved in this database copy.
    /// Zero is a represented value, not evidence of a missing field.
    pub fn saved_media_anchor(&self) -> u32 {
        self.saved_media_anchor
    }

    /// Dataset 11's third u32, saved VoiceMemo hostLast, not a Ready value.
    pub fn saved_voice_memo_anchor(&self) -> u32 {
        self.saved_voice_memo_anchor
    }

    /// Root +0x48 database PID, independently verified by the catalog writer.
    /// This is distinct from the preference-derived HostInfo LibraryID.
    pub fn database_pid(&self) -> u64 {
        self.database_pid
    }
}

#[derive(Clone, Copy)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u64(self, input: &[u8], offset: usize) -> Result<u64> {
        let bytes: [u8; 8] = input
            .get(offset..offset.checked_add(8).context("field offset overflow")?)
            .context("truncated database field")?
            .try_into()
            .expect("checked eight-byte field");
        Ok(match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        })
    }
    fn u32(self, input: &[u8], offset: usize) -> Result<u32> {
        let bytes = input
            .get(offset..offset.checked_add(4).context("field offset overflow")?)
            .context("truncated database field")?;
        let bytes: [u8; 4] = bytes.try_into().expect("checked four-byte field");
        Ok(match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        })
    }
}

/// Accepts only the observed current root header and version: 1.117 with no
/// compression or 2.117 with one zlib stream following the root header.
/// Every top-level section is bounded before the saved anchor is returned.
pub fn parse(input: &[u8]) -> Result<AnchorState> {
    ensure!(
        input.len() <= MAX_INPUT_BYTES,
        "database exceeds 64 MiB input limit"
    );
    ensure!(
        input.len() >= ROOT_HEADER_BYTES,
        "truncated database root header"
    );
    let order = match &input[..4] {
        b"mhbd" => ByteOrder::Little,
        b"dbhm" => ByteOrder::Big,
        _ => bail!("unsupported database magic"),
    };
    ensure!(
        order.u32(input, 4)? as usize == ROOT_HEADER_BYTES,
        "unsupported database root header length"
    );
    ensure!(
        order.u32(input, 8)? as usize == input.len(),
        "database declared file length does not match input"
    );
    ensure!(
        order.u32(input, 0x10)? == 117,
        "unsupported database minor version"
    );
    let count = order.u32(input, 0x14)?;
    ensure!(
        (1..=MAX_SECTIONS).contains(&count),
        "database section count is absent or exceeds limit"
    );
    let sections = &input[ROOT_HEADER_BYTES..];
    let anchors = match (order.u32(input, 0x0c)?, input[0xa8]) {
        (1, 0) => parse_sections(sections, order, count),
        (2, 1) => {
            let inflated = inflate_sections(sections, MAX_SECTION_BYTES)?;
            parse_sections(&inflated, order, count)
        }
        _ => bail!("unsupported database major version/compression combination"),
    }?;
    Ok(AnchorState {
        saved_media_anchor: anchors[0],
        saved_voice_memo_anchor: anchors[2],
        database_pid: order.u64(input, 0x48)?,
    })
}

fn parse_sections(input: &[u8], order: ByteOrder, count: u32) -> Result<[u32; 4]> {
    ensure!(
        input.len() <= MAX_SECTION_BYTES,
        "database sections exceed size limit"
    );
    let mut offset = 0usize;
    let mut saved_anchors = None;
    for _ in 0..count {
        let section = input
            .get(offset..)
            .context("section offset exceeds database")?;
        ensure!(section.len() >= 16, "truncated database section header");
        ensure!(
            order.u32(section, 0)? == 0x6473686d,
            "invalid database section magic"
        );
        let header_len = order.u32(section, 4)? as usize;
        let total_len = order.u32(section, 8)? as usize;
        ensure!(header_len >= 16, "database section header is too short");
        ensure!(
            total_len >= header_len,
            "database section total is smaller than header"
        );
        ensure!(
            total_len <= section.len(),
            "database section exceeds remaining input"
        );
        if order.u32(section, 12)? == 11 {
            ensure!(saved_anchors.is_none(), "duplicate saved-anchor dataset 11");
            ensure!(
                header_len == ANCHOR_HEADER_BYTES
                    && total_len == ANCHOR_HEADER_BYTES + ANCHOR_PAYLOAD_BYTES,
                "unsupported saved-anchor dataset 11 layout"
            );
            // Native writer/callback/initializer map these slots to Media,
            // Ringtone, VoiceMemo, Book. Remaining payload stays uninterpreted.
            saved_anchors = Some([
                order.u32(section, header_len)?,
                order.u32(section, header_len + 4)?,
                order.u32(section, header_len + 8)?,
                order.u32(section, header_len + 12)?,
            ]);
        }
        offset = offset
            .checked_add(total_len)
            .context("section offset overflow")?;
    }
    ensure!(offset == input.len(), "trailing database sections or bytes");
    saved_anchors.context("saved-anchor dataset 11 is absent")
}

fn inflate_sections(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = Decompress::new(true);
    let mut output = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        // One extra byte detects an over-limit stream even when the output
        // exactly fills the allowed space before its checksum is consumed.
        let available = (limit - output.len()).saturating_add(1).min(chunk.len());
        let status = decoder
            .decompress(
                &input[before_in as usize..],
                &mut chunk[..available],
                FlushDecompress::None,
            )
            .context("invalid database zlib stream")?;
        let produced = (decoder.total_out() - before_out) as usize;
        ensure!(
            produced <= limit - output.len(),
            "inflated database exceeds size limit"
        );
        output
            .try_reserve_exact(produced)
            .context("allocating bounded database sections")?;
        output.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            ensure!(
                decoder.total_in() == input.len() as u64,
                "trailing bytes or multiple database zlib streams"
            );
            return Ok(output);
        }
        ensure!(
            decoder.total_in() > before_in || produced > 0,
            "truncated or stalled database zlib stream"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn put_u32(input: &mut [u8], offset: usize, value: u32, order: ByteOrder) {
        let bytes = match order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        input[offset..offset + 4].copy_from_slice(&bytes);
    }

    fn section(kind: u32, payload: &[u8], order: ByteOrder) -> Vec<u8> {
        let mut bytes = vec![0; ANCHOR_HEADER_BYTES + payload.len()];
        let len = bytes.len() as u32;
        put_u32(&mut bytes, 0, 0x6473686d, order);
        put_u32(&mut bytes, 4, ANCHOR_HEADER_BYTES as u32, order);
        put_u32(&mut bytes, 8, len, order);
        put_u32(&mut bytes, 12, kind, order);
        bytes[ANCHOR_HEADER_BYTES..].copy_from_slice(payload);
        bytes
    }

    fn anchor_section(anchor: u32, order: ByteOrder) -> Vec<u8> {
        let mut payload = [0; ANCHOR_PAYLOAD_BYTES];
        for (index, value) in [anchor, 29, 31, 37].into_iter().enumerate() {
            put_u32(&mut payload, index * 4, value, order);
        }
        section(11, &payload, order)
    }

    fn zlib(input: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn database(sections: &[Vec<u8>], order: ByteOrder, compressed: bool) -> Vec<u8> {
        let body = sections.concat();
        let body = if compressed { zlib(&body) } else { body };
        let mut bytes = vec![0; ROOT_HEADER_BYTES];
        bytes.extend_from_slice(&body);
        let len = bytes.len() as u32;
        put_u32(&mut bytes, 0, 0x6462686d, order);
        put_u32(&mut bytes, 4, ROOT_HEADER_BYTES as u32, order);
        put_u32(&mut bytes, 8, len, order);
        put_u32(&mut bytes, 12, if compressed { 2 } else { 1 }, order);
        put_u32(&mut bytes, 16, 117, order);
        put_u32(&mut bytes, 20, sections.len() as u32, order);
        bytes[0xa8] = u8::from(compressed);
        bytes
    }

    fn example() -> Vec<u8> {
        database(
            &[anchor_section(17, ByteOrder::Little)],
            ByteOrder::Little,
            false,
        )
    }

    fn error(bytes: &[u8]) -> String {
        match parse(bytes) {
            Err(error) => format!("{error:#}"),
            Ok(_) => panic!("malformed database accepted"),
        }
    }

    #[test]
    fn reads_saved_media_voice_and_database_pid_in_both_byte_orders_and_containers() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            for compressed in [false, true] {
                let mut bytes = database(
                    &[
                        section(1, b"uninspected track data", order),
                        anchor_section(0x1234abcd, order),
                        section(12, b"uninspected other data", order),
                    ],
                    order,
                    compressed,
                );
                bytes[0x48..0x50].copy_from_slice(&match order {
                    ByteOrder::Little => 0x8172_6354_4536_2718u64.to_le_bytes(),
                    ByteOrder::Big => 0x8172_6354_4536_2718u64.to_be_bytes(),
                });
                let parsed = parse(&bytes).unwrap();
                assert_eq!(parsed.saved_media_anchor(), 0x1234abcd);
                assert_eq!(parsed.saved_voice_memo_anchor(), 31);
                assert_eq!(parsed.database_pid(), 0x8172_6354_4536_2718);
            }
        }
    }

    #[test]
    fn preserves_zero_and_maximum_saved_values() {
        for anchor in [0, u32::MAX] {
            let bytes = database(
                &[anchor_section(anchor, ByteOrder::Little)],
                ByteOrder::Little,
                false,
            );
            assert_eq!(parse(&bytes).unwrap().saved_media_anchor(), anchor);
        }
    }

    #[test]
    fn rejects_truncated_roots_and_oversized_input() {
        let bytes = example();
        for len in 0..ROOT_HEADER_BYTES {
            assert!(parse(&bytes[..len]).is_err());
        }
        assert!(error(&vec![0; MAX_INPUT_BYTES + 1]).contains("input limit"));
    }

    #[test]
    fn rejects_root_magic_lengths_and_versions() {
        for (offset, value, message) in [
            (0, 0, "magic"),
            (4, 0xf0, "header length"),
            (8, 0, "file length"),
            (12, 0, "major version"),
            (12, 2, "major version"),
            (16, 116, "minor version"),
            (20, 0, "section count"),
            (20, MAX_SECTIONS + 1, "section count"),
        ] {
            let mut bytes = example();
            put_u32(&mut bytes, offset, value, ByteOrder::Little);
            assert!(error(&bytes).contains(message), "offset {offset}");
        }
        for flag in [1, 2, 255] {
            let mut bytes = example();
            bytes[0xa8] = flag;
            assert!(error(&bytes).contains("compression"));
        }
    }

    #[test]
    fn rejects_section_magic_truncation_and_impossible_lengths() {
        for (offset, value, message) in [
            (0, 0, "section magic"),
            (4, 0, "too short"),
            (4, 145, "smaller than header"),
            (8, 95, "smaller than header"),
            (8, u32::MAX, "exceeds remaining"),
            (4, 16, "dataset 11 layout"),
        ] {
            let mut bytes = example();
            put_u32(
                &mut bytes,
                ROOT_HEADER_BYTES + offset,
                value,
                ByteOrder::Little,
            );
            assert!(error(&bytes).contains(message), "offset {offset}");
        }
        for payload_len in [0, 15, 16, 47, 49] {
            let bytes = database(
                &[section(11, &vec![0; payload_len], ByteOrder::Little)],
                ByteOrder::Little,
                false,
            );
            assert!(error(&bytes).contains("dataset 11 layout"));
        }
    }

    #[test]
    fn rejects_missing_duplicate_and_hidden_extra_sections() {
        let order = ByteOrder::Little;
        let bytes = database(&[section(1, &[], order)], order, false);
        assert!(error(&bytes).contains("absent"));
        let bytes = database(
            &[anchor_section(17, order), anchor_section(17, order)],
            order,
            false,
        );
        assert!(error(&bytes).contains("duplicate"));
        let mut bytes = database(
            &[anchor_section(17, order), section(1, &[], order)],
            order,
            false,
        );
        put_u32(&mut bytes, 20, 1, order);
        assert!(error(&bytes).contains("trailing"));
        let mut bytes = example();
        put_u32(&mut bytes, 20, 2, order);
        assert!(error(&bytes).contains("truncated database section"));
    }

    #[test]
    fn validates_sections_after_anchor_before_returning() {
        let order = ByteOrder::Little;
        let mut bytes = database(
            &[anchor_section(17, order), section(1, &[], order)],
            order,
            false,
        );
        put_u32(&mut bytes, ROOT_HEADER_BYTES + 144 + 8, u32::MAX, order);
        assert!(error(&bytes).contains("exceeds remaining"));
    }

    #[test]
    fn inflation_is_bounded_and_accepts_exact_limit() {
        let input = vec![0x41; 100_000];
        let compressed = zlib(&input);
        assert_eq!(inflate_sections(&compressed, input.len()).unwrap(), input);
        assert!(inflate_sections(&compressed, input.len() - 1).is_err());
        assert!(inflate_sections(&compressed, 0).is_err());
    }

    #[test]
    fn rejects_truncated_corrupt_and_trailing_zlib_streams() {
        let compressed = zlib(&anchor_section(17, ByteOrder::Little));
        for length in 0..compressed.len() {
            assert!(
                inflate_sections(&compressed[..length], 1024).is_err(),
                "prefix {length}"
            );
        }
        let mut corrupt = compressed.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(inflate_sections(&corrupt, 1024).is_err());
        for suffix in [vec![0], compressed.clone(), zlib(&[])] {
            let mut trailing = compressed.clone();
            trailing.extend(suffix);
            assert!(inflate_sections(&trailing, 1024).is_err());
        }
    }

    #[test]
    fn rejects_compressed_trailing_data_even_with_consistent_file_size() {
        let order = ByteOrder::Little;
        let mut bytes = database(&[anchor_section(17, order)], order, true);
        bytes.push(0);
        let len = bytes.len() as u32;
        put_u32(&mut bytes, 8, len, order);
        assert!(error(&bytes).contains("trailing"));
    }
}
