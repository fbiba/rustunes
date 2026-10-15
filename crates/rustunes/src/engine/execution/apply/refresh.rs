//! Ordinary insertion refresh checks. A fresh capture is a new before state,
//! never a preservation waiver or an instruction to retry a Started operation.
use crate::{
    engine::execution::native_insert::{Control, PreSessionStateDrift},
    engine::planning::source_collection::{self, SourceCollection},
    engine::state::current::Current,
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

pub(super) struct FrozenIntent {
    content: crate::engine::state::journal::Content,
    operation: Vec<u8>,
    new_album: Option<u64>,
    new_artist: Option<u64>,
    anchor: u32,
    selected: crate::engine::planning::collections::PlannedTrack,
    timestamp: std::time::SystemTime,
}
impl FrozenIntent {
    pub(super) fn new(
        p: &crate::engine::state::journal::PreparedAttempt,
        selected: &crate::engine::planning::collections::PlannedTrack,
        timestamp: std::time::SystemTime,
    ) -> Self {
        Self {
            content: p.content.clone(),
            operation: p.operation.clone(),
            new_album: p.new_album_pid,
            new_artist: p.new_artist_pid,
            anchor: p.current_media_anchor,
            selected: selected.clone(),
            timestamp,
        }
    }
    pub(super) fn revalidate(
        &self,
        p: &crate::engine::state::journal::PreparedAttempt,
        source: &crate::engine::planning::source_collection::SourceTrack,
        catalog: &Catalog,
        reserved: &std::collections::BTreeSet<u64>,
        control: &Control,
    ) -> Result<Vec<u8>> {
        control.check()?;
        ensure!(
            p.kind == crate::engine::state::journal::AttemptKind::Insert
                && p.content == self.content
                && p.content.source == source.fingerprint
                && p.operation == self.operation
                && p.new_album_pid == self.new_album
                && p.new_artist_pid == self.new_artist
                && p.current_media_anchor == self.anchor,
            "refresh changed frozen insertion content, identities, allocation or operation"
        );
        let mut planner = crate::engine::planning::collections::Planner::new(catalog, reserved);
        let selected = planner.plan(
            &source.metadata,
            crate::engine::planning::collections::CandidateIds {
                track: self.content.track_pid,
                album: Some(self.selected.album_pid()),
                artist: Some(self.selected.artist_pid()),
            },
        )?;
        ensure!(
            selected.track_pid() == self.selected.track_pid()
                && selected.album_pid() == self.selected.album_pid()
                && selected.artist_pid() == self.selected.artist_pid()
                && selected.new_collection_ids() == self.selected.new_collection_ids()
                && selected.pending_collection_ids() == self.selected.pending_collection_ids(),
            "refresh changed original allocated identities or collection decisions"
        );
        let rebuilt = crate::engine::execution::insert::prepare_live(
            &source.metadata,
            &selected,
            catalog,
            self.timestamp,
        )?;
        ensure!(
            rebuilt == self.operation,
            "refresh changed immutable insertion operation; no alternate intent will be sent"
        );
        control.check()?;
        Ok(rebuilt)
    }
}

pub(super) fn may_refresh(error: &anyhow::Error, attempt: u8, control: &Control) -> Result<bool> {
    control.check()?;
    Ok(attempt == 1 && error.downcast_ref::<PreSessionStateDrift>().is_some())
}

pub(super) async fn controlled<T>(
    future: impl std::future::Future<Output = Result<T>>,
    control: &Control,
) -> Result<T> {
    control.check()?;
    tokio::select! {
        value = future => { control.check()?; value }
        error = async { loop { tokio::time::sleep(std::time::Duration::from_millis(50)).await; control.check()?; } } => error,
    }
}

pub(super) fn ensure_current(before: &Current, fresh: &Current) -> Result<()> {
    ensure!(
        before.device == fresh.device
            && before.identity.device_digest() == fresh.identity.device_digest()
            && before.identity.library_digest() == fresh.identity.library_digest()
            && before.raw_preferences == fresh.raw_preferences
            && before.plist_preferences == fresh.plist_preferences
            && before.cdb == fresh.cdb
            && before.artwork_db == fresh.artwork_db,
        "refresh changed device, library, preferences, CDB or artwork bytes"
    );
    before.library.ensure_matches(&fresh.library)?;
    ensure_catalog(&before.catalog, &fresh.catalog)
}

fn ensure_catalog(before: &Catalog, fresh: &Catalog) -> Result<()> {
    ensure!(
        before.database_pid() == fresh.database_pid()
            && before.primary_container_pid() == fresh.primary_container_pid()
            && before.persisted_media_anchor() == fresh.persisted_media_anchor()
            && before.tracks() == fresh.tracks()
            && before.albums() == fresh.albums()
            && before.artists() == fresh.artists()
            && before.composers() == fresh.composers()
            && before.track_composers() == fresh.track_composers()
            && before.stats_except_recent_play() == fresh.stats_except_recent_play()
            && before.represented_sync_ids() == fresh.represented_sync_ids(),
        "refresh changed the frozen catalogue planning basis"
    );
    for track in before.tracks() {
        ensure!(
            before.title_sort(track.sync_id) == fresh.title_sort(track.sync_id),
            "refresh changed a nullable sort title"
        );
    }
    let a = before.artwork();
    let b = fresh.artwork();
    let counts = |a: music_protocol::catalogue::sqlite::ArtworkRowCounts| {
        (a.cached_rows, a.registration_rows)
    };
    ensure!(
        a.occupied_cache_ids() == b.occupied_cache_ids()
            && a.cached_ids() == b.cached_ids()
            && a.cached_objects() == b.cached_objects()
            && a.track_cache_ids() == b.track_cache_ids()
            && a.tracks_with_artwork_registration() == b.tracks_with_artwork_registration()
            && a.album_cache_ids() == b.album_cache_ids()
            && counts(a.source300_rows()) == counts(b.source300_rows())
            && a.other_source_rows()
                .iter()
                .map(|(k, v)| (*k, counts(*v)))
                .collect::<Vec<_>>()
                == b.other_source_rows()
                    .iter()
                    .map(|(k, v)| (*k, counts(*v)))
                    .collect::<Vec<_>>()
            && a.unmapped_source300_token_rows() == b.unmapped_source300_token_rows()
            && a.orphan_album_cache_ids() == b.orphan_album_cache_ids(),
        "refresh changed artwork registrations or cache occupancy"
    );
    Ok(())
}

pub(super) fn ensure_sources(frozen: &SourceCollection, control: &Control) -> Result<()> {
    control.check()?;
    frozen.ensure_unambiguous()?;
    let selected = source_collection::select(&frozen.root, &frozen.options)?;
    let fresh = source_collection::inspect_with_progress(&selected, |_, _| control.check())?;
    fresh.ensure_unambiguous()?;
    ensure!(
        fresh.root == frozen.root
            && fresh.base_directory == frozen.base_directory
            && fresh.tracks.len() == frozen.tracks.len()
            && fresh.selection_stats.selected_files == frozen.selection_stats.selected_files
            && fresh.selection_stats.selected_bytes == frozen.selection_stats.selected_bytes,
        "source selection changed during pre-session refresh"
    );
    for (a, b) in frozen.tracks.iter().zip(&fresh.tracks) {
        ensure!(
            a.relative_path == b.relative_path
                && a.fingerprint == b.fingerprint
                && a.metadata == b.metadata,
            "selected source bytes or metadata changed during refresh"
        );
    }
    let exclusions = |s: &SourceCollection| {
        s.exclusions
            .iter()
            .map(|x| (x.relative_path.clone(), x.reason, x.descendants_unscanned))
            .collect::<Vec<_>>()
    };
    ensure!(
        exclusions(frozen) == exclusions(&fresh),
        "source exclusion set changed during refresh"
    );
    control.check()
}

/// Copy only the fixed files consumed by retained insertion recovery. The
/// original capture/previous verified after is never changed or relabelled.
pub(super) fn copy_before(
    source: &Path,
    target: &Path,
    current: &Current,
    control: &Control,
) -> Result<()> {
    control.check()?;
    fs::DirBuilder::new().mode(0o700).create(target)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(target.join("catalog"))?;
    let h = current.catalog.source_hashes();
    for (name, hash, size) in [
        (
            "catalog/MediaLibrary.sqlitedb",
            h.database,
            h.database_bytes,
        ),
        ("state-receipt.json", current.receipt_sha256, 1024 * 1024),
        (
            "iTunesPrefs",
            Sha256::digest(&current.raw_preferences).into(),
            current.raw_preferences.len() as u64,
        ),
        (
            "iTunesPrefs.plist",
            Sha256::digest(&current.plist_preferences).into(),
            current.plist_preferences.len() as u64,
        ),
        (
            "iTunesCDB",
            Sha256::digest(&current.cdb).into(),
            current.cdb.len() as u64,
        ),
    ] {
        copy_file(&source.join(name), &target.join(name), hash, size, control)?;
    }
    if let Some(hash) = h.wal {
        copy_file(
            &source.join("catalog/MediaLibrary.sqlitedb-wal"),
            &target.join("catalog/MediaLibrary.sqlitedb-wal"),
            hash,
            h.wal_bytes.context("WAL size missing")?,
            control,
        )?;
    } else {
        ensure!(
            !source
                .join("catalog/MediaLibrary.sqlitedb-wal")
                .try_exists()?,
            "unbound WAL beside before capture"
        );
    }
    File::open(target.join("catalog"))?.sync_all()?;
    File::open(target)?.sync_all()?;
    File::open(target.parent().context("before parent missing")?)?.sync_all()?;
    control.check()
}

fn copy_file(
    source: &Path,
    target: &Path,
    expected: [u8; 32],
    cap: u64,
    control: &Control,
) -> Result<()> {
    ensure!(
        cap <= music_protocol::protocol::transfer::MAX_CATALOG,
        "retained before file exceeds bound"
    );
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)?;
    let before = input.metadata()?;
    ensure!(
        before.is_file() && before.nlink() == 1 && before.len() <= cap,
        "unsupported retained before file"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    let mut sha = Sha256::new();
    let mut total = 0u64;
    let mut chunk = [0; 65536];
    loop {
        control.check()?;
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .context("capture size overflow")?;
        ensure!(total <= cap, "before file grew past bound");
        sha.update(&chunk[..n]);
        output.write_all(&chunk[..n])?;
    }
    let after = input.metadata()?;
    let path = fs::symlink_metadata(source)?;
    let identity = |m: &fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    ensure!(
        identity(&before) == identity(&after)
            && identity(&before) == identity(&path)
            && total == before.len()
            && <[u8; 32]>::from(sha.finalize()) == expected,
        "retained before file changed or differs from captured digest"
    );
    output.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
