//! Bounded, read-only inventory of saved native ArtworkDB image records.
//!
//! AMPDevicesAgent 1.6.4.94 (SHA256 f5e0f2115283e8719df383baf59d9b8a6f5a2ec39e566fbe802357f2d11e6fa9)
//! reader 0x4d7700 -> 0x4d68dc and writer 0x4da140 -> 0x4d8bf8 -> 0x4d886c
//! establish the framing and ID fields. ArtworkDB is host-written saved state:
//! successful parsing or counter validation does NOT establish freshness.

use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGES: usize = 100_000;
const MAX_OBJECTS: usize = 500_000;
const ROOT_HEADER: usize = 132;
const SECTION_HEADER: usize = 96;
const LIST_HEADER: usize = 92;
const IMAGE_HEADER: usize = 152;

#[derive(Debug)]
pub struct Inventory {
    pub next_record_id: u32,
    pub minor_version: u32,
    /// Cache record ID -> source track PID. The PID is a reference, not an owner grant.
    pub images: BTreeMap<u32, u64>,
    pub section_types: BTreeSet<u16>,
}

impl Inventory {
    /// Validate the native saved counter against parsed and independently
    /// reconciled IDs. The caller must supply all additional observed/journaled
    /// IDs and establish snapshot freshness before using this candidate.
    /// This never repairs or advances a stale counter, and never writes a DB.
    pub fn candidate_with_occupied(&self, occupied: &BTreeSet<u32>) -> Result<u32> {
        ensure!(
            (100..u32::MAX).contains(&self.next_record_id),
            "artwork counter is reserved or would overflow on allocation"
        );
        ensure!(
            self.images
                .keys()
                .chain(occupied)
                .all(|id| *id < self.next_record_id),
            "artwork counter does not exceed every reconciled occupied ID"
        );
        Ok(self.next_record_id)
    }
}

#[derive(Clone, Copy)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u16(self, data: &[u8], at: usize) -> Result<u16> {
        let raw = field::<2>(data, at)?;
        Ok(match self {
            Self::Little => u16::from_le_bytes(raw),
            Self::Big => u16::from_be_bytes(raw),
        })
    }
    fn u32(self, data: &[u8], at: usize) -> Result<u32> {
        let raw = field::<4>(data, at)?;
        Ok(match self {
            Self::Little => u32::from_le_bytes(raw),
            Self::Big => u32::from_be_bytes(raw),
        })
    }
    fn u64(self, data: &[u8], at: usize) -> Result<u64> {
        let raw = field::<8>(data, at)?;
        Ok(match self {
            Self::Little => u64::from_le_bytes(raw),
            Self::Big => u64::from_be_bytes(raw),
        })
    }
    fn magic(self, data: &[u8], expected: &[u8; 4]) -> Result<()> {
        let mut got = field::<4>(data, 0)?;
        if matches!(self, Self::Big) {
            got.reverse();
        }
        ensure!(&got == expected, "unsupported ArtworkDB record magic");
        Ok(())
    }
}

fn field<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N]> {
    Ok(data
        .get(at..at.checked_add(N).context("ArtworkDB offset overflow")?)
        .context("truncated ArtworkDB field")?
        .try_into()
        .expect("checked field length"))
}

fn header(order: Order, data: &[u8], minimum: usize) -> Result<usize> {
    let size = order.u32(data, 4)? as usize;
    ensure!(
        size >= minimum && size <= data.len(),
        "invalid ArtworkDB header size"
    );
    Ok(size)
}

fn record<'a>(
    order: Order,
    data: &'a [u8],
    magic: &[u8; 4],
    minimum: usize,
) -> Result<(&'a [u8], usize)> {
    order.magic(data, magic)?;
    let head = header(order, data, minimum)?;
    let total = order.u32(data, 8)? as usize;
    ensure!(
        total >= head && total <= data.len() && total.is_multiple_of(4),
        "invalid ArtworkDB record size"
    );
    Ok((&data[..total], head))
}

pub fn parse(data: &[u8]) -> Result<Inventory> {
    ensure!(
        (ROOT_HEADER..=MAX_BYTES).contains(&data.len()),
        "ArtworkDB size outside bound"
    );
    let order = match data.get(..4) {
        Some(b"mhfd") => Order::Little,
        Some(b"dfhm") => Order::Big,
        _ => anyhow::bail!("unsupported ArtworkDB root magic"),
    };
    let (root, mut offset) = record(order, data, b"mhfd", ROOT_HEADER)?;
    ensure!(
        root.len() == data.len(),
        "ArtworkDB file size differs from root size"
    );
    ensure!(
        order.u32(root, 12)? == 0,
        "unsupported ArtworkDB major version"
    );
    let section_count = order.u32(root, 20)? as usize;
    ensure!(
        (1..=3).contains(&section_count),
        "unsupported ArtworkDB section count"
    );
    let mut result = Inventory {
        next_record_id: order.u32(root, 28)?,
        minor_version: order.u32(root, 16)?,
        images: BTreeMap::new(),
        section_types: BTreeSet::new(),
    };
    let mut object_budget = MAX_OBJECTS;
    for _ in 0..section_count {
        let (section, section_header) = record(order, &root[offset..], b"mhsd", SECTION_HEADER)?;
        let kind = order.u16(section, 12)?;
        ensure!(
            (1..=3).contains(&kind) && result.section_types.insert(kind),
            "unknown or duplicate ArtworkDB section"
        );
        if kind == 1 {
            let list = &section[section_header..];
            order.magic(list, b"mhli")?;
            let mut at = header(order, list, LIST_HEADER)?;
            let count = order.u32(list, 8)? as usize;
            ensure!(count <= MAX_IMAGES, "ArtworkDB image limit exceeded");
            for _ in 0..count {
                let (image, image_header) = record(order, &list[at..], b"mhii", IMAGE_HEADER)?;
                let id = order.u32(image, 16)?;
                ensure!(id != 0, "zero ArtworkDB image ID is an allocation sentinel");
                let source = order.u64(image, 20)?;
                ensure!(
                    result.images.insert(id, source).is_none(),
                    "duplicate ArtworkDB image ID"
                );
                let children = order.u32(image, 12)? as usize;
                ensure!(children <= object_budget, "ArtworkDB object limit exceeded");
                object_budget -= children;
                let mut child_at = image_header;
                for _ in 0..children {
                    let (child, _) = record(order, &image[child_at..], b"mhod", 24)?;
                    child_at += child.len();
                }
                ensure!(
                    child_at == image.len(),
                    "ArtworkDB image child count/size mismatch"
                );
                at += image.len();
            }
            ensure!(at == list.len(), "ArtworkDB image list count/size mismatch");
        }
        // Album and format sections carry separate structures. Their envelope
        // is checked; opaque payloads are not scanned for apparent image IDs.
        offset += section.len();
    }
    ensure!(
        offset == root.len(),
        "ArtworkDB section count/size mismatch"
    );
    ensure!(
        result.section_types.contains(&1),
        "ArtworkDB image section absent"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put32(data: &mut [u8], at: usize, value: u32, big: bool) {
        data[at..at + 4].copy_from_slice(&if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        });
    }
    fn block(magic: &[u8; 4], head: usize, total: usize, big: bool) -> Vec<u8> {
        let mut data = vec![0; total];
        let mut magic = *magic;
        if big {
            magic.reverse();
        }
        data[..4].copy_from_slice(&magic);
        put32(&mut data, 4, head as u32, big);
        put32(&mut data, 8, total as u32, big);
        data
    }
    fn fixture(big: bool, ids: &[u32], next: u32) -> Vec<u8> {
        let mut images = Vec::new();
        for id in ids {
            let mut image = block(b"mhii", IMAGE_HEADER, IMAGE_HEADER + 28, big);
            put32(&mut image, 12, 1, big);
            put32(&mut image, 16, *id, big);
            let pid = 0xfedcba9876543210u64;
            image[20..28].copy_from_slice(&if big {
                pid.to_be_bytes()
            } else {
                pid.to_le_bytes()
            });
            image[IMAGE_HEADER..].copy_from_slice(&block(b"mhod", 24, 28, big));
            images.extend(image);
        }
        let mut list = block(b"mhli", LIST_HEADER, LIST_HEADER, big);
        put32(&mut list, 8, ids.len() as u32, big);
        list.extend(images);
        let mut section = block(b"mhsd", SECTION_HEADER, SECTION_HEADER, big);
        section[12..14].copy_from_slice(&if big {
            1u16.to_be_bytes()
        } else {
            1u16.to_le_bytes()
        });
        section.extend(list);
        let len = section.len();
        put32(&mut section, 8, len as u32, big);
        let mut root = block(b"mhfd", ROOT_HEADER, ROOT_HEADER, big);
        put32(&mut root, 16, 6, big);
        put32(&mut root, 20, 1, big);
        put32(&mut root, 28, next, big);
        root.extend(section);
        let len = root.len();
        put32(&mut root, 8, len as u32, big);
        root
    }
    #[test]
    fn reads_both_orders_and_validates_external_counter_collisions() {
        for big in [false, true] {
            let inv = parse(&fixture(big, &[100, 675], 677)).unwrap();
            assert_eq!(inv.images.len(), 2);
            assert_eq!(inv.images[&675], 0xfedcba9876543210);
            assert_eq!(inv.minor_version, 6);
            assert_eq!(
                inv.candidate_with_occupied(&BTreeSet::from([0, 676]))
                    .unwrap(),
                677
            );
            assert!(inv.candidate_with_occupied(&BTreeSet::from([677])).is_err());
            assert!(inv.candidate_with_occupied(&BTreeSet::from([678])).is_err());
        }
    }
    #[test]
    fn rejects_reserved_overflow_stale_and_duplicate_ids() {
        for next in [0, 99, 100, 675, u32::MAX] {
            assert!(
                parse(&fixture(false, &[100, 675], next))
                    .unwrap()
                    .candidate_with_occupied(&BTreeSet::new())
                    .is_err()
            );
        }
        assert!(parse(&fixture(false, &[100, 100], 101)).is_err());
        assert!(parse(&fixture(false, &[0], 100)).is_err());
    }
    #[test]
    fn rejects_every_truncation_and_framing_escape() {
        let valid = fixture(false, &[100], 101);
        for n in 0..valid.len() {
            assert!(parse(&valid[..n]).is_err(), "cut {n}");
        }
        for offset in [
            4,
            8,
            20,
            ROOT_HEADER + 4,
            ROOT_HEADER + 8,
            ROOT_HEADER + SECTION_HEADER + 4,
            ROOT_HEADER + SECTION_HEADER + 8,
            ROOT_HEADER + SECTION_HEADER + LIST_HEADER + 4,
            ROOT_HEADER + SECTION_HEADER + LIST_HEADER + 8,
            ROOT_HEADER + SECTION_HEADER + LIST_HEADER + 12,
        ] {
            let mut data = valid.clone();
            put32(&mut data, offset, u32::MAX, false);
            assert!(parse(&data).is_err(), "field {offset}");
        }
        let mut trailing = valid.clone();
        trailing.extend([0; 4]);
        assert!(parse(&trailing).is_err());
    }
    #[test]
    fn rejects_unsupported_version_section_and_hidden_child_bytes() {
        let valid = fixture(false, &[100], 101);
        for (at, value) in [
            (12, 1),
            (ROOT_HEADER + 12, 4),
            (ROOT_HEADER + SECTION_HEADER + LIST_HEADER + 12, 0),
        ] {
            let mut data = valid.clone();
            put32(&mut data, at, value, false);
            assert!(parse(&data).is_err());
        }
        let mut data = valid.clone();
        data[ROOT_HEADER + SECTION_HEADER + LIST_HEADER] = b'x';
        assert!(parse(&data).is_err());
    }
}
