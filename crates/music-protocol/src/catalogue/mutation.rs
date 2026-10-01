//! Pure native title update and ordinary deletion envelopes for tracks,
//! unfilled rows and playlists.
//!
//! These bytes do not establish ownership or authorize deletion. The caller
//! selects permitted targets, binds a fresh catalog/identity/readback snapshot,
//! checks the same anchor in Ready, and journals the batch before staging.
//! Evidence: docs/05-receiver-operations.md, sections 2 to 4.

use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};

use crate::catalogue::sqlite::{Catalog, Track};

/// Change exactly catalog title and sort_name. The caller supplies both literal
/// values: no article stripping, source-file rewrite, or statistics update.
pub fn title(
    catalog: &Catalog,
    track_sync_id: u64,
    title: &str,
    sort_name: &str,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    prepare(
        catalog_identity(catalog),
        target(catalog.tracks(), track_sync_id)?,
        Mutation::Title { title, sort_name },
        timestamp,
    )
}

/// Ordinary one-track delete, not Reset or filesystem removal. Serialization
/// alone proves neither receiver acceptance nor reclamation of files/collections.
pub fn delete(catalog: &Catalog, track_sync_id: u64, timestamp: SystemTime) -> Result<Vec<u8>> {
    prepare(
        catalog_identity(catalog),
        target(catalog.tracks(), track_sync_id)?,
        Mutation::Delete,
        timestamp,
    )
}

/// Several ordinary deletes in one session, applied by the receiver in this
/// order. Like [`delete`], the bytes prove nothing; the caller verifies the
/// complete removal afterwards.
pub fn delete_many(
    catalog: &Catalog,
    track_sync_ids: &[u64],
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let tracks = track_sync_ids
        .iter()
        .map(|id| target(catalog.tracks(), *id))
        .collect::<Result<Vec<_>>>()?;
    prepare_many(
        catalog_identity(catalog),
        &tracks,
        timestamp,
        validate_removable,
    )
}

/// The same batch for rows an interrupted insertion left without audio: each
/// is a library row with no file location and no asset. Removing them is the
/// compensation for that insertion.
pub fn delete_many_unfilled(
    catalog: &Catalog,
    track_sync_ids: &[u64],
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let tracks = track_sync_ids
        .iter()
        .map(|id| target(catalog.tracks(), *id))
        .collect::<Result<Vec<_>>>()?;
    prepare_many(
        catalog_identity(catalog),
        &tracks,
        timestamp,
        validate_unfilled,
    )
}

/// Ordinary playlists deleted in one session, one bare `delete_playlist` per
/// container sync id, after `update_db_info`, in this order. The bytes prove
/// nothing; the caller selects eligible playlists and verifies the deletion.
pub fn delete_playlists(
    catalog: &Catalog,
    containers: &[u64],
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    prepare_playlists(catalog_identity(catalog), containers, timestamp)
}

fn prepare_playlists(
    catalog: CatalogIdentity,
    containers: &[u64],
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    ensure!(
        catalog.database != 0 && catalog.primary_container != 0,
        "missing current catalog identity"
    );
    ensure!(!containers.is_empty(), "no playlists to delete");
    let revision = catalog
        .anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut operations = Vec::with_capacity(containers.len() + 1);
    operations.push(db_info_update(&catalog));
    for container in containers {
        ensure!(
            *container != 0 && *container != catalog.primary_container && seen.insert(*container),
            "playlist {container:016x} is zero, the library container, or listed twice"
        );
        let mut operation = Dictionary::new();
        operation.insert("operation".into(), "delete_playlist".into());
        operation.insert("pid".into(), (*container as i64).into());
        operations.push(Value::Dictionary(operation));
    }
    envelope(revision, timestamp, operations)
}

fn validate_unfilled(track: &Track) -> Result<()> {
    let state = &track.availability;
    ensure!(
        track.sync_id != 0
            && track.afc_path.is_none()
            && !state.is_placeholder
            && state.is_library_added
            && !state.has_non_purgeable_asset
            && state.is_supported_type,
        "track {:016x} is not a library row without audio",
        track.sync_id
    );
    Ok(())
}

fn prepare_many(
    catalog: CatalogIdentity,
    tracks: &[&Track],
    timestamp: SystemTime,
    validate: fn(&Track) -> Result<()>,
) -> Result<Vec<u8>> {
    ensure!(
        catalog.database != 0 && catalog.primary_container != 0,
        "missing current catalog identity"
    );
    ensure!(!tracks.is_empty(), "no tracks to delete");
    let revision = catalog
        .anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut operations = Vec::with_capacity(tracks.len() + 1);
    operations.push(db_info_update(&catalog));
    for track in tracks {
        ensure!(
            seen.insert(track.sync_id),
            "track {:016x} listed twice",
            track.sync_id
        );
        validate(track)?;
        let mut operation = Dictionary::new();
        operation.insert("operation".into(), "delete_track".into());
        operation.insert("pid".into(), (track.sync_id as i64).into());
        operations.push(Value::Dictionary(operation));
    }
    envelope(revision, timestamp, operations)
}

fn db_info_update(catalog: &CatalogIdentity) -> Value {
    let mut db_info = Dictionary::new();
    db_info.insert(
        "primary_container_pid".into(),
        (catalog.primary_container as i64).into(),
    );
    let mut db_update = Dictionary::new();
    db_update.insert("pid".into(), (catalog.database as i64).into());
    db_update.insert("operation".into(), "update_db_info".into());
    db_update.insert("db_info".into(), Value::Dictionary(db_info));
    Value::Dictionary(db_update)
}

fn envelope(revision: u32, timestamp: SystemTime, operations: Vec<Value>) -> Result<Vec<u8>> {
    crate::protocol::operations::ensure_within(operations.len())?;
    let mut envelope = Dictionary::new();
    envelope.insert("revision".into(), revision.into());
    envelope.insert("timestamp".into(), Value::Date(timestamp.into()));
    envelope.insert("operations".into(), Value::Array(operations));
    let mut bytes = Vec::new();
    Value::Dictionary(envelope).to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

struct CatalogIdentity {
    database: u64,
    primary_container: u64,
    anchor: u32,
}
enum Mutation<'a> {
    Title { title: &'a str, sort_name: &'a str },
    Delete,
}

fn catalog_identity(catalog: &Catalog) -> CatalogIdentity {
    CatalogIdentity {
        database: catalog.database_pid(),
        primary_container: catalog.primary_container_pid(),
        anchor: catalog.persisted_media_anchor(),
    }
}

fn target(tracks: &[Track], sync_id: u64) -> Result<&Track> {
    ensure!(sync_id != 0, "mutation target sync identity is zero");
    let mut matches = tracks.iter().filter(|track| track.sync_id == sync_id);
    let track = matches
        .next()
        .context("mutation target is absent from catalog")?;
    ensure!(matches.next().is_none(), "mutation target is ambiguous");
    Ok(track)
}

fn prepare(
    catalog: CatalogIdentity,
    track: &Track,
    mutation: Mutation<'_>,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    ensure!(
        catalog.database != 0 && catalog.primary_container != 0,
        "missing current catalog identity"
    );
    let revision = catalog
        .anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    validate_local_track(track)?;
    encode(catalog, track.sync_id, mutation, timestamp, revision)
}

// Both profiles use the same exact envelope encoding after their distinct
// target validation. This helper grants no protocol or journal authority.
fn encode(
    catalog: CatalogIdentity,
    track_sync_id: u64,
    mutation: Mutation<'_>,
    timestamp: SystemTime,
    revision: u32,
) -> Result<Vec<u8>> {
    let db_update = db_info_update(&catalog);
    let mut operation = Dictionary::new();
    match mutation {
        Mutation::Title { title, sort_name } => {
            for text in [title, sort_name] {
                ensure!(
                    !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
                    "invalid mutation title/sort-name text"
                );
            }
            operation.insert("operation".into(), "update_track".into());
            operation.insert("pid".into(), (track_sync_id as i64).into());
            let mut item = Dictionary::new();
            item.insert("title".into(), title.into());
            item.insert("sort_name".into(), sort_name.into());
            operation.insert("item".into(), Value::Dictionary(item));
        }
        Mutation::Delete => {
            operation.insert("operation".into(), "delete_track".into());
            operation.insert("pid".into(), (track_sync_id as i64).into());
        }
    }
    envelope(
        revision,
        timestamp,
        vec![db_update, Value::Dictionary(operation)],
    )
}

fn validate_local_track(track: &Track) -> Result<()> {
    validate_removable(track)?;
    ensure!(
        track.metadata.file_size > 0 && track.metadata.total_time_ms > 0,
        "mutation target has empty audio metadata"
    );
    Ok(())
}

/// A batch delete names the row only, so its stored audio metadata does not
/// matter; availability and the local Music path still do.
fn validate_removable(track: &Track) -> Result<()> {
    let state = &track.availability;
    ensure!(
        track.sync_id != 0
            && !state.is_placeholder
            && state.is_library_added
            && state.has_non_purgeable_asset
            && state.is_supported_type,
        "mutation target is not an available local Music track"
    );
    let path = track
        .afc_path
        .as_deref()
        .context("mutation target has no permanent path")?;
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4 && parts[..2] == ["iTunes_Control", "Music"],
        "mutation target is not a local Music path"
    );
    let bucket = parts[2].as_bytes();
    ensure!(
        bucket.len() == 3 && bucket[0] == b'F' && bucket[1..].iter().all(u8::is_ascii_digit),
        "unsupported mutation target folder"
    );
    let leaf = parts[3];
    ensure!(
        !leaf.is_empty()
            && leaf.len() <= 255
            && leaf != "."
            && leaf != ".."
            && leaf
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "unsafe mutation target filename"
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

    fn track(id: u64) -> Track {
        Track {
            sync_id: id,
            internal_id: 1,
            album_sync_id: 21,
            artist_sync_id: 31,
            playback: Default::default(),
            metadata: TrackMetadata {
                content_rating: 0,
                content_rating_level: 0,
                is_compilation: false,
                title: "Original".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                album_artist: String::new(),
                genre: None,
                composer: None,
                copyright: None,
                track_number: 1,
                track_count: 1,
                disc_number: 1,
                disc_count: 1,
                year: 0,
                total_time_ms: 3000,
                file_size: 52152,
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
    fn envelope(bytes: &[u8], revision: u64, time: SystemTime) -> Vec<Value> {
        assert!(bytes.starts_with(b"bplist00"));
        let value = Value::from_reader(Cursor::new(bytes)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(
            keys(root),
            BTreeSet::from(["revision", "timestamp", "operations"])
        );
        assert_eq!(root["revision"].as_unsigned_integer(), Some(revision));
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
        operations.clone()
    }
}
