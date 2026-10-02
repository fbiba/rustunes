//! Prepare the native artwork-only update for an existing local Music track.
//!
//! The caller separately proves source/track ownership, reserves a collision-free
//! cache record, and checks capture hashes and the live Ready anchor. This pure
//! builder does not allocate cache IDs, inspect an ArtworkDB, or send anything.
//! Every cover update observed on the phone staged update_track dictionaries
//! with only these two artwork item keys.

use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};

use crate::{
    catalogue::sqlite::{Catalog, Track},
    media::artwork::Cover,
};

const MAX_COVER_BYTES: usize = 32 * 1024 * 1024;

/// Serialize the ordinary envelope at persisted anchor + 1. Sign and send these
/// exact bytes only after the caller's independent live-session preflight.
pub fn prepare(
    catalog: &Catalog,
    track_sync_id: u64,
    cache_id: u32,
    cover: &Cover,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let track = target(catalog.tracks(), track_sync_id)?;
    operation(
        CatalogIdentity {
            database: catalog.database_pid(),
            primary_container: catalog.primary_container_pid(),
            anchor: catalog.persisted_media_anchor(),
        },
        track,
        cache_id,
        cover,
        timestamp,
    )
}

fn target(tracks: &[Track], sync_id: u64) -> Result<&Track> {
    ensure!(sync_id != 0, "artwork target sync identity is zero");
    let mut matches = tracks.iter().filter(|track| track.sync_id == sync_id);
    let first = matches
        .next()
        .context("artwork target is absent from catalog")?;
    ensure!(matches.next().is_none(), "artwork target is ambiguous");
    Ok(first)
}

struct CatalogIdentity {
    database: u64,
    primary_container: u64,
    anchor: u32,
}

fn operation(
    catalog: CatalogIdentity,
    track: &Track,
    cache_id: u32,
    cover: &Cover,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    ensure!(
        catalog.database != 0 && catalog.primary_container != 0,
        "artwork catalog identity is absent"
    );
    let revision = catalog
        .anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    ensure!(cache_id >= 100, "artwork cache identity is reserved");
    ensure!(track.sync_id != 0, "artwork target sync identity is zero");
    let availability = &track.availability;
    ensure!(
        !availability.is_placeholder
            && availability.is_library_added
            && availability.has_non_purgeable_asset
            && availability.is_supported_type,
        "artwork target is not an available supported library track"
    );
    ensure!(
        track.metadata.file_size > 0 && track.metadata.total_time_ms > 0,
        "artwork target audio metadata is empty"
    );
    local_music_path(
        track
            .afc_path
            .as_deref()
            .context("artwork target has no local media path")?,
    )?;
    validate_cover(cover)?;

    let mut db_info = Dictionary::new();
    db_info.insert(
        "primary_container_pid".into(),
        (catalog.primary_container as i64).into(),
    );
    let mut db_update = Dictionary::new();
    db_update.insert("pid".into(), (catalog.database as i64).into());
    db_update.insert("operation".into(), "update_db_info".into());
    db_update.insert("db_info".into(), Value::Dictionary(db_info));

    let mut item = Dictionary::new();
    item.insert("artwork_cache_id".into(), cache_id.into());
    item.insert("artwork_checksum".into(), cover.checksum.into());
    let mut update = Dictionary::new();
    update.insert("operation".into(), "update_track".into());
    update.insert("pid".into(), (track.sync_id as i64).into());
    update.insert("item".into(), Value::Dictionary(item));

    let mut envelope = Dictionary::new();
    envelope.insert("revision".into(), revision.into());
    envelope.insert("timestamp".into(), Value::Date(timestamp.into()));
    envelope.insert(
        "operations".into(),
        Value::Array(vec![
            Value::Dictionary(db_update),
            Value::Dictionary(update),
        ]),
    );
    let mut bytes = Vec::new();
    Value::Dictionary(envelope).to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

fn validate_cover(cover: &Cover) -> Result<()> {
    ensure!(
        (4..=MAX_COVER_BYTES).contains(&cover.bytes.len()),
        "artwork JPEG length outside supported range"
    );
    ensure!(
        cover.bytes.starts_with(&[0xff, 0xd8]) && cover.bytes.ends_with(&[0xff, 0xd9]),
        "artwork JPEG markers are invalid"
    );
    let digest: [u8; 32] = Sha256::digest(&cover.bytes).into();
    ensure!(
        digest == cover.sha256,
        "artwork bytes do not match their digest"
    );
    let checksum = u32::try_from(cover.bytes.len())?
        .checked_add(17)
        .context("artwork checksum overflow")?;
    ensure!(
        cover.checksum == checksum,
        "artwork checksum does not match the single JPEG data atom"
    );
    Ok(())
}

fn local_music_path(path: &str) -> Result<()> {
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4 && parts[..2] == ["iTunes_Control", "Music"],
        "artwork target is not a local Music path"
    );
    let bucket = parts[2].as_bytes();
    ensure!(
        bucket.len() == 3 && bucket[0] == b'F' && bucket[1..].iter().all(u8::is_ascii_digit),
        "artwork target Music bucket is unsupported"
    );
    let leaf = parts[3];
    ensure!(
        !leaf.is_empty()
            && leaf.len() <= 255
            && leaf != "."
            && leaf != ".."
            && leaf
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "artwork target Music filename is unsafe"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::sqlite::{Availability, TrackMetadata};
    use std::{
        collections::BTreeSet,
        io::Cursor,
        time::{Duration, UNIX_EPOCH},
    };

    fn track(sync_id: u64) -> Track {
        Track {
            sync_id,
            internal_id: 1,
            album_sync_id: 21,
            artist_sync_id: 31,
            playback: Default::default(),
            metadata: TrackMetadata {
                content_rating: 0,
                content_rating_level: 0,
                is_compilation: false,
                title: "Synthetic".into(),
                artist: "Synthetic artist".into(),
                album: "Synthetic album".into(),
                album_artist: "Synthetic artist".into(),
                genre: None,
                composer: None,
                copyright: None,
                track_number: 1,
                track_count: 1,
                disc_number: 1,
                disc_count: 0,
                year: 2026,
                total_time_ms: 1000,
                file_size: 1234,
            },
            afc_path: Some("iTunes_Control/Music/F39/TEST.m4a".into()),
            availability: Availability {
                is_placeholder: false,
                is_library_added: true,
                has_non_purgeable_asset: true,
                is_supported_type: true,
            },
        }
    }
    fn cover() -> Cover {
        let bytes = vec![0xff, 0xd8, 0xff, 0xd9];
        Cover {
            sha256: Sha256::digest(&bytes).into(),
            bytes,
            checksum: 21,
        }
    }
    fn identity(anchor: u32) -> CatalogIdentity {
        CatalogIdentity {
            database: u64::MAX - 1,
            primary_container: u64::MAX - 2,
            anchor,
        }
    }
    fn keys(dictionary: &Dictionary) -> BTreeSet<&str> {
        dictionary.keys().map(String::as_str).collect()
    }

    #[test]
    fn emits_exact_artwork_only_shape_and_existing_signed_identities() {
        let time = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let bytes = operation(identity(12), &track(u64::MAX), 100, &cover(), time).unwrap();
        assert!(bytes.starts_with(b"bplist00"));
        let value = Value::from_reader(Cursor::new(&bytes)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(
            keys(root),
            BTreeSet::from(["revision", "timestamp", "operations"])
        );
        assert_eq!(root["revision"].as_unsigned_integer(), Some(13));
        assert_eq!(root["timestamp"].as_date(), Some(time.into()));
        let operations = root["operations"].as_array().unwrap();
        assert_eq!(operations.len(), 2);
        let db = operations[0].as_dictionary().unwrap();
        assert_eq!(keys(db), BTreeSet::from(["operation", "pid", "db_info"]));
        assert_eq!(db["operation"].as_string(), Some("update_db_info"));
        assert_eq!(db["pid"].as_signed_integer(), Some(-2));
        let info = db["db_info"].as_dictionary().unwrap();
        assert_eq!(keys(info), BTreeSet::from(["primary_container_pid"]));
        assert_eq!(info["primary_container_pid"].as_signed_integer(), Some(-3));
        let update = operations[1].as_dictionary().unwrap();
        assert_eq!(keys(update), BTreeSet::from(["operation", "pid", "item"]));
        assert_eq!(update["operation"].as_string(), Some("update_track"));
        assert_eq!(update["pid"].as_signed_integer(), Some(-1));
        let item = update["item"].as_dictionary().unwrap();
        assert_eq!(
            keys(item),
            BTreeSet::from(["artwork_cache_id", "artwork_checksum"])
        );
        assert_eq!(item["artwork_cache_id"].as_unsigned_integer(), Some(100));
        assert_eq!(item["artwork_checksum"].as_unsigned_integer(), Some(21));
        assert_eq!(
            bytes,
            operation(identity(12), &track(u64::MAX), 100, &cover(), time).unwrap()
        );
    }

    #[test]
    fn absent_zero_and_ambiguous_targets_fail() {
        assert!(target(&[track(1)], 2).is_err());
        assert!(target(&[track(0)], 0).is_err());
        assert!(target(&[track(1), track(1)], 1).is_err());
    }

    #[test]
    fn reserved_cache_ids_and_exhausted_anchor_fail() {
        for cache in [0, 1, 99] {
            assert!(operation(identity(12), &track(1), cache, &cover(), UNIX_EPOCH).is_err());
        }
        assert!(operation(identity(u32::MAX), &track(1), 100, &cover(), UNIX_EPOCH).is_err());
    }

    #[test]
    fn unavailable_nonlocal_and_empty_audio_targets_fail() {
        for change in 0..8 {
            let mut t = track(1);
            match change {
                0 => t.availability.is_placeholder = true,
                1 => t.availability.is_library_added = false,
                2 => t.availability.has_non_purgeable_asset = false,
                3 => t.availability.is_supported_type = false,
                4 => t.afc_path = None,
                5 => t.afc_path = Some("https://example.invalid/audio.m4a".into()),
                6 => t.metadata.file_size = 0,
                _ => t.metadata.total_time_ms = 0,
            }
            assert!(operation(identity(12), &t, 100, &cover(), UNIX_EPOCH).is_err());
        }
        assert!(local_music_path("iTunes_Control/Music/F39/../TEST.m4a").is_err());
    }

    #[test]
    fn empty_corrupt_digest_and_checksum_covers_fail() {
        for change in 0..5 {
            let mut c = cover();
            match change {
                0 => c.bytes.clear(),
                1 => c.bytes[0] = 0,
                2 => c.sha256[0] ^= 1,
                3 => c.checksum = 0,
                _ => c.checksum += 1,
            }
            assert!(operation(identity(12), &track(1), 100, &c, UNIX_EPOCH).is_err());
        }
    }
}
