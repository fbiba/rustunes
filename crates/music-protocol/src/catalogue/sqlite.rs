//! Read-only catalog from a bounded local MediaLibrary database/WAL capture.
//!
//! Read from user_version 2400060 on, when the tables and columns in `TABLES`
//! are present. Wire identities come from sync_id, not SQLite item_pid or
//! item_state.sync_id.
//! This module performs no device I/O and never opens a source file with SQLite.

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags, Row,
    config::DbConfig,
    hooks::{AuthAction, Authorization},
    limits::Limit,
    types::ValueRef,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub const MAX_DATABASE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_WAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ROWS: usize = 50_000;
const MAX_TOTAL_ROWS: usize = 200_000;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 64 * 1024 * 1024;
/// Lowest MediaLibrary user_version read. Older databases are refused.
pub const USER_VERSION_FLOOR: i64 = 2_400_060;
/// The user_versions this reader was qualified against on a phone. Any other
/// version at or above the floor is read, and `Catalog` reports it untested.
pub const TESTED_USER_VERSIONS: &[i64] = &[2_400_060, 2_410_000];
static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
mod scratch;
pub mod tables;

#[derive(Clone)]
pub struct SourceHashes {
    pub database: [u8; 32],
    pub wal: Option<[u8; 32]>,
    pub database_bytes: u64,
    pub wal_bytes: Option<u64>,
}

pub struct Catalog {
    source_hashes: SourceHashes,
    user_version: i64,
    database_pid: u64,
    primary_container_pid: u64,
    anchor: u32,
    tracks: Vec<Track>,
    albums: BTreeMap<i64, Album>,
    artists: BTreeMap<i64, Artist>,
    unsynced_albums: BTreeMap<i64, Album>,
    unsynced_artists: BTreeMap<i64, Artist>,
    composers: BTreeMap<i64, ComposerLookup>,
    track_composers: BTreeMap<u64, ComposerReference>,
    title_sorts: BTreeMap<u64, Option<String>>,
    comments: BTreeMap<u64, Option<String>>,
    item_artists: BTreeMap<i64, ItemArtist>,
    track_item_artists: BTreeMap<u64, i64>,
    stats_except_recent_play: BTreeMap<u64, [u8; 32]>,
    stats_except_recent_play_and_skip: BTreeMap<u64, [u8; 32]>,
    stats_except_playback: BTreeMap<u64, [u8; 32]>,
    playback_dates: BTreeMap<u64, PlaybackDates>,
    represented_sync_ids: BTreeSet<u64>,
    music_folders: BTreeSet<String>,
    artwork: ArtworkInventory,
}

impl Catalog {
    /// The database's `PRAGMA user_version`.
    pub fn user_version(&self) -> i64 {
        self.user_version
    }
    /// Whether `user_version` is in `TESTED_USER_VERSIONS`.
    pub fn user_version_tested(&self) -> bool {
        TESTED_USER_VERSIONS.contains(&self.user_version)
    }
    /// The phone's own registry of audio folders (`iTunes_Control/Music/Fnn`
    /// rows of `base_location`). It keeps them when no track uses them.
    pub fn music_folders(&self) -> &BTreeSet<String> {
        &self.music_folders
    }
    pub fn source_hashes(&self) -> &SourceHashes {
        &self.source_hashes
    }
    pub fn database_pid(&self) -> u64 {
        self.database_pid
    }
    pub fn primary_container_pid(&self) -> u64 {
        self.primary_container_pid
    }
    /// Persisted state only; a negotiated Ready response remains independent.
    pub fn persisted_media_anchor(&self) -> u32 {
        self.anchor
    }
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }
    pub fn albums(&self) -> &BTreeMap<i64, Album> {
        &self.albums
    }
    /// Wire artist collections are stored in album_artist, not item_artist.
    pub fn artists(&self) -> &BTreeMap<i64, Artist> {
        &self.artists
    }
    /// Albums no sync made (`sync_id` 0), by internal id: the phone files the
    /// entries it adds outside the library under them. No operation can name
    /// one, so `albums` leaves them out. They count no members; `artist_sync_id`
    /// is 0 when the album artist has no sync identity either.
    pub fn unsynced_albums(&self) -> &BTreeMap<i64, Album> {
        &self.unsynced_albums
    }
    /// Album artists no sync made (`sync_id` 0), by internal id, left out of
    /// `artists` as `unsynced_albums` are left out of `albums`.
    pub fn unsynced_artists(&self) -> &BTreeMap<i64, Artist> {
        &self.unsynced_artists
    }
    /// Exact composer lookup labels and sort labels, including orphan rows.
    pub fn composers(&self) -> &BTreeMap<i64, ComposerLookup> {
        &self.composers
    }
    /// Every track's composer lookup reference and derived order value.
    pub fn track_composers(&self) -> &BTreeMap<u64, ComposerReference> {
        &self.track_composers
    }
    /// Exact nullable item_extra.sort_title, keyed by track sync identity.
    /// Kept separate from serialized Track/index data. None means no such track;
    /// Some(None) preserves SQL NULL rather than inferring the displayed title.
    pub fn title_sort(&self, track: u64) -> Option<&Option<String>> {
        self.title_sorts.get(&track)
    }
    /// Exact nullable item_extra.comment, keyed by track sync identity, kept
    /// apart from Track as the title sort is.
    pub fn comment(&self, track: u64) -> Option<&Option<String>> {
        self.comments.get(&track)
    }
    /// Every `item_artist` row, by internal id.
    pub fn item_artists(&self) -> &BTreeMap<i64, ItemArtist> {
        &self.item_artists
    }
    /// The `item_artist` row a track is filed under.
    pub fn track_item_artist(&self, track: u64) -> Option<&ItemArtist> {
        self.item_artists.get(self.track_item_artists.get(&track)?)
    }
    /// The stored sort name of the track artist `name` would join: the rows
    /// with that name or its ASCII-case variant. Several rows with different
    /// sort names are ambiguous.
    pub fn item_artist_sort_for_name(&self, name: &str) -> Result<Option<&str>> {
        let mut sorts = self
            .item_artists
            .values()
            .filter(|row| {
                row.name == name
                    || (row.name.is_ascii()
                        && name.is_ascii()
                        && row.name.eq_ignore_ascii_case(name))
            })
            .filter_map(|row| row.sort_name.as_deref());
        let found = sorts.next();
        ensure!(
            sorts.all(|other| Some(other) == found),
            "track artists named {name:?} have different sort names"
        );
        Ok(found)
    }
    /// Exact typed item_stats row fingerprints, excluding only play_count_recent.
    /// Includes unprojected dates, bookmark fields and SQL column names/types;
    /// this is preservation evidence, not a decoded interpretation of those fields.
    pub fn stats_except_recent_play(&self) -> &BTreeMap<u64, [u8; 32]> {
        &self.stats_except_recent_play
    }
    /// Separate artwork-import profile beside `stats_except_recent_play`:
    /// this excludes exactly the two recent counters, retaining every other
    /// raw column, value and SQLite storage type.
    pub fn stats_except_recent_play_and_skip(&self) -> &BTreeMap<u64, [u8; 32]> {
        &self.stats_except_recent_play_and_skip
    }
    /// The profile for checks taken after the phone was used: excludes the
    /// seven columns listening changes (`PLAYBACK_COLUMNS`), retaining every
    /// other raw column, value and SQLite storage type.
    pub fn stats_except_playback(&self) -> &BTreeMap<u64, [u8; 32]> {
        &self.stats_except_playback
    }
    /// Each track's last play and skip dates, as stored.
    pub fn playback_dates(&self) -> &BTreeMap<u64, PlaybackDates> {
        &self.playback_dates
    }

    /// Resolve one exact or ASCII-case-equivalent lookup; never choose among
    /// duplicate rows or infer Unicode/grouping-key equivalence.
    pub fn composer_for_label(&self, name: &str) -> Result<Option<&ComposerLookup>> {
        let mut matches = self.composers.values().filter(|row| {
            row.name == name
                || (row.name.is_ascii() && name.is_ascii() && row.name.eq_ignore_ascii_case(name))
        });
        let found = matches.next();
        ensure!(
            matches.next().is_none(),
            "ambiguous existing composer lookup"
        );
        Ok(found)
    }
    /// Nonzero sync IDs in the mapped item/collection/container tables, plus
    /// db_info identities. No claim covers opaque properties or historical IDs.
    pub fn represented_sync_ids(&self) -> &BTreeSet<u64> {
        &self.represented_sync_ids
    }
    /// Observed local-art source300 evidence, not a cache allocator or proof
    /// that any corresponding artwork file exists on the phone.
    pub fn artwork(&self) -> &ArtworkInventory {
        &self.artwork
    }
    /// Fails if any represented row lacks a supported catalog location.
    pub fn music_paths(&self) -> Result<BTreeSet<String>> {
        self.tracks
            .iter()
            .map(|t| t.afc_path.clone().context("catalog track location absent"))
            .collect()
    }
}

/// An `item_artist` row: the track artists, which the phone files by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemArtist {
    pub internal_id: i64,
    pub name: String,
    pub sort_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ComposerLookup {
    pub internal_id: i64,
    pub sync_id: u64,
    pub name: String,
    pub sort_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ComposerReference {
    /// None represents the native zero composer_pid sentinel.
    pub lookup_id: Option<i64>,
    pub order: i64,
}

/// Counts preserve the distinction between cached objects and entity relations.
#[derive(Clone, Copy, Default)]
pub struct ArtworkRowCounts {
    pub cached_rows: usize,
    pub registration_rows: usize,
}

/// Exact source300 cached-object row. SQL presence is not file presence.
#[derive(Clone, PartialEq, Eq)]
pub struct CachedArtwork {
    pub cache_id: u32,
    pub artwork_type: i64,
    pub variant: i64,
    /// Canonical observed shard/name suffix; no cache root is implied here.
    pub relative_path: String,
}

#[derive(Clone)]
pub struct ArtworkInventory {
    occupied_cache_ids: BTreeSet<u32>,
    cached_ids: BTreeSet<u32>,
    cached_objects: BTreeMap<(u32, i64), CachedArtwork>,
    track_cache_ids: BTreeMap<u64, u32>,
    tracks_with_artwork_registration: BTreeSet<u64>,
    album_cache_ids: BTreeMap<u64, u32>,
    source300_rows: ArtworkRowCounts,
    other_source_rows: BTreeMap<i64, ArtworkRowCounts>,
    unmapped_source300_token_rows: usize,
    orphan_album_cache_ids: BTreeMap<i64, u32>,
}

impl ArtworkInventory {
    /// Union of canonical source300 IDs in BOTH artwork and artwork_token.
    /// Historical cached IDs and other relation shapes are never omitted.
    pub fn occupied_cache_ids(&self) -> &BTreeSet<u32> {
        &self.occupied_cache_ids
    }
    /// IDs with source300 SQL artwork rows. This does not prove file presence,
    /// available raster dimensions, or a valid AFC path.
    pub fn cached_ids(&self) -> &BTreeSet<u32> {
        &self.cached_ids
    }
    /// Source300 cached objects keyed by (cache ID, variant).
    /// Paths are bounded canonical 2hex/38hex suffixes, not AFC paths.
    pub fn cached_objects(&self) -> &BTreeMap<(u32, i64), CachedArtwork> {
        &self.cached_objects
    }
    /// Track wire sync ID -> observed source300/entity0/type1/variant0 cache ID.
    /// Absence means no registration of this exact supported shape.
    pub fn track_cache_ids(&self) -> &BTreeMap<u64, u32> {
        &self.track_cache_ids
    }
    /// Track wire IDs with any entity0 artwork-token registration, across all
    /// sources, artwork types and variants. Presence is conservative: it does
    /// not interpret foreign tokens as cache IDs or prove cached file bytes.
    pub fn tracks_with_artwork_registration(&self) -> &BTreeSet<u64> {
        &self.tracks_with_artwork_registration
    }
    /// Album wire sync ID -> observed source300/entity4/type6/variant0 cache ID.
    pub fn album_cache_ids(&self) -> &BTreeMap<u64, u32> {
        &self.album_cache_ids
    }
    pub fn source300_rows(&self) -> ArtworkRowCounts {
        self.source300_rows
    }
    /// Counts only: other-source tokens are not interpreted as native cache IDs.
    pub fn other_source_rows(&self) -> &BTreeMap<i64, ArtworkRowCounts> {
        &self.other_source_rows
    }
    /// Other source300 relation shapes, and the registrations of items and
    /// albums no sync made, still contribute every ID to occupancy.
    pub fn unmapped_source300_token_rows(&self) -> usize {
        self.unmapped_source300_token_rows
    }
    /// Album entity pid -> cache ID of an album registration whose album row
    /// is gone. A removal session that empties albums may delete their rows
    /// and keep these tokens (observed); the ID stays occupied.
    pub fn orphan_album_cache_ids(&self) -> &BTreeMap<i64, u32> {
        &self.orphan_album_cache_ids
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub sync_id: u64,
    pub internal_id: i64,
    pub album_sync_id: u64,
    pub artist_sync_id: u64,
    pub metadata: TrackMetadata,
    pub afc_path: Option<String>,
    pub availability: Availability,
    pub playback: PlaybackStatistics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Catalog grouping label; does not establish whether source aART exists.
    pub album_artist: String,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub copyright: Option<String>,
    /// Exact item_extra code; no source-tag or rating-system interpretation.
    pub content_rating: u32,
    pub content_rating_level: u32,
    /// Exact item flag, independent of the album's all_compilations summary.
    pub is_compilation: bool,
    pub track_number: u32,
    pub track_count: u32,
    pub disc_number: u32,
    pub disc_count: u32,
    pub year: u32,
    pub total_time_ms: u32,
    pub file_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    pub is_placeholder: bool,
    pub is_library_added: bool,
    pub has_non_purgeable_asset: bool,
    pub is_supported_type: bool,
}

/// The item_stats columns completed plays and skips change.
pub const PLAYBACK_COLUMNS: [&str; 7] = [
    "play_count_user",
    "play_count_recent",
    "has_been_played",
    "date_played",
    "skip_count_user",
    "skip_count_recent",
    "date_skipped",
];

/// A track's `date_played` and `date_skipped` (seconds since 2001; the phone
/// stores some as reals). Zero when null or when the columns are absent; NaN
/// when not a number.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackDates {
    pub played: f64,
    pub skipped: f64,
}

/// Exact item_stats values from the same read-only catalog snapshot.
/// User and recent counters remain separate; neither is recomputed from dates.
/// Fractional playback dates, bookmarks and other statistics are not exposed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStatistics {
    pub play_count_user: u32,
    pub play_count_recent: u32,
    pub skip_count_user: u32,
    pub skip_count_recent: u32,
    pub has_been_played: bool,
    /// Exact numeric code, without converting to a star rating.
    pub user_rating: u32,
    pub user_rating_is_derived: bool,
    /// Exact numeric code, without inferring a liked/disliked enum.
    pub liked_state: u32,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Album {
    pub internal_id: i64,
    pub sync_id: u64,
    pub name: String,
    /// Exact nullable `sort_album`.
    pub sort_name: Option<String>,
    pub artist_internal_id: i64,
    pub artist_sync_id: u64,
    pub live_track_count: usize,
    pub all_compilations: bool,
    /// Derived from live item rows, independently of the album summary flag.
    pub has_compilation_tracks: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Artist {
    pub internal_id: i64,
    pub sync_id: u64,
    pub name: String,
    /// Exact nullable `sort_album_artist`.
    pub sort_name: Option<String>,
    pub live_track_count: usize,
}

/// Analyze a stable capture. None means the caller observed WAL absence;
/// Some(empty file) records a present but empty WAL. Capture consistency across
/// AFC files remains the caller's responsibility. Sources must be regular files.
pub fn read_snapshot(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
) -> Result<Catalog> {
    read_snapshot_checked(database, wal, scratch_parent, None)
}
/// Read with an explicit shared cancellation flag and absolute deadline.
/// Policy is checked during private copying, SQLite work and result collection.
pub fn read_snapshot_with_control(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Catalog> {
    let control = ReadControl {
        deadline,
        cancelled,
    };
    read_snapshot_checked(database, wal, scratch_parent, Some(&control))
}

/// Analyze a saved snapshot with memory-only SQLite temporary storage while
/// preserving the legacy 64 MiB database and 256 MiB WAL input bounds. Uses the
/// caller's original deadline/cancellation through copying, SQL and rechecks.
pub fn read_snapshot_memory_with_control(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Catalog> {
    let control = ReadControl {
        deadline,
        cancelled,
    };
    read_snapshot_checked_options(
        database,
        wal,
        scratch_parent,
        Some(&control),
        SnapshotStorage::MemoryTemp,
    )
}

pub fn read_snapshot_current_with_control(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Catalog> {
    let control = ReadControl {
        deadline,
        cancelled,
    };
    control.check()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    ensure!(
        page_size > 0 && (page_size as u64).is_power_of_two() && page_size <= 1024 * 1024,
        "Current SQLite OS page size exceeds storage profile"
    );
    ensure!(
        fs::metadata(database)?.len() <= 32 * 1024 * 1024,
        "Current catalogue database exceeds 32MiB"
    );
    if let Some(wal) = wal {
        ensure!(
            fs::metadata(wal)?.len() <= 32 * 1024 * 1024,
            "Current catalogue WAL exceeds 32MiB"
        );
    }
    read_snapshot_checked_options(
        database,
        wal,
        scratch_parent,
        Some(&control),
        SnapshotStorage::Current,
    )
}

fn read_snapshot_checked(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
    control: Option<&ReadControl>,
) -> Result<Catalog> {
    read_snapshot_checked_options(
        database,
        wal,
        scratch_parent,
        control,
        SnapshotStorage::Legacy,
    )
}
#[derive(Clone, Copy)]
enum SnapshotStorage {
    Legacy,
    MemoryTemp,
    Current,
}
impl SnapshotStorage {
    fn input_limits(self) -> (u64, u64) {
        match self {
            Self::Current => (32 * 1024 * 1024, 32 * 1024 * 1024),
            Self::Legacy | Self::MemoryTemp => (MAX_DATABASE_BYTES, MAX_WAL_BYTES),
        }
    }
    fn memory_temp(self) -> bool {
        !matches!(self, Self::Legacy)
    }
}

fn read_snapshot_checked_options(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
    control: Option<&ReadControl>,
    storage: SnapshotStorage,
) -> Result<Catalog> {
    check_control(control)?;
    let (catalog, sql_phase) = scratch::with_scratch(scratch_parent, control, |scratch| {
        let hash = |source: &Path, limit| match control {
            None => hash_file(source, limit),
            Some(_) => hash_file_checked(source, limit, control),
        };
        let db_path = scratch.path().join("MediaLibrary.sqlitedb");
        let wal_path = scratch.path().join("MediaLibrary.sqlitedb-wal");
        let (database_limit, wal_limit) = storage.input_limits();
        let db = scratch.copy(database, "MediaLibrary.sqlitedb", database_limit, control)?;
        let journal = wal
            .map(|p| scratch.copy(p, "MediaLibrary.sqlitedb-wal", wal_limit, control))
            .transpose()?;
        match control {
            None => validate_container(&db_path, journal.as_ref().map(|_| wal_path.as_path()))?,
            Some(_) => validate_container_checked(
                &db_path,
                journal.as_ref().map(|_| wal_path.as_path()),
                control,
            )?,
        }
        scratch.prepare_sql(journal.map(|(_, bytes)| bytes), control)?;
        scratch.check(control)?;
        check_control(control)?;
        let hashes = SourceHashes {
            database: db.0,
            database_bytes: db.1,
            wal: journal.as_ref().map(|r| r.0),
            wal_bytes: journal.as_ref().map(|r| r.1),
        };
        let (catalog, sql_phase) = match control {
            None => {
                let connection = open_readonly(&db_path)?;
                scratch.check(control)?;
                let catalog = parse_catalog(&connection, hashes, None)?;
                scratch.check(control)?;
                (catalog, None)
            }
            Some(control) => {
                let (connection, phase) = if storage.memory_temp() {
                    open_readonly_with_control_options(&db_path, control, true)?
                } else {
                    open_readonly_with_control(&db_path, control)?
                };
                scratch.check(Some(control))?;
                let catalog = parse_catalog(&connection, hashes, Some(&phase))?;
                phase.check()?;
                scratch.check(Some(control))?;
                (catalog, Some(phase))
            }
        };
        scratch.check(control)?;
        ensure!(
            hash(database, MAX_DATABASE_BYTES)? == db,
            "source database changed during analysis"
        );
        if let (Some(source), Some(expected)) = (wal, journal) {
            ensure!(
                hash(source, MAX_WAL_BYTES)? == expected,
                "source WAL changed during analysis"
            );
        }
        check_control(control)?;
        check_control(sql_phase.as_ref())?;
        Ok((catalog, sql_phase))
    })?;
    // The original SQL phase also covers the final held-copy verification.
    check_control(sql_phase.as_ref())?;
    Ok(catalog)
}

fn source_file(path: &Path, limit: u64) -> Result<(File, fs::Metadata)> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "snapshot source is a symlink"
    );
    let file = File::open(path).context("open local catalog capture")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit,
        "snapshot source is not a bounded regular file"
    );
    Ok((file, metadata))
}

fn unchanged(before: &fs::Metadata, after: &fs::Metadata) -> Result<()> {
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "snapshot source changed during read"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "snapshot source identity changed"
        );
    }
    Ok(())
}

fn hash_file(path: &Path, limit: u64) -> Result<([u8; 32], u64)> {
    hash_file_checked(path, limit, None)
}
fn hash_file_checked(
    path: &Path,
    limit: u64,
    control: Option<&ReadControl>,
) -> Result<([u8; 32], u64)> {
    check_control(control)?;
    let (mut file, before) = source_file(path, limit)?;
    check_control(control)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65_536];
    let mut count = 0u64;
    loop {
        check_control(control)?;
        let size = file.read(&mut buffer)?;
        check_control(control)?;
        if size == 0 {
            break;
        }
        count += size as u64;
        ensure!(count <= limit, "snapshot exceeds byte limit");
        digest.update(&buffer[..size]);
    }
    unchanged(&before, &file.metadata()?)?;
    unchanged(&before, &fs::metadata(path)?)?;
    ensure!(count == before.len(), "snapshot read length differs");
    check_control(control)?;
    Ok((digest.finalize().into(), count))
}

fn copy_source_checked(
    source: &Path,
    output: &mut File,
    limit: u64,
    control: Option<&ReadControl>,
) -> Result<([u8; 32], u64)> {
    check_control(control)?;
    let (mut input, before) = source_file(source, limit)?;
    check_control(control)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65_536];
    let mut count = 0u64;
    loop {
        check_control(control)?;
        let size = input.read(&mut buffer)?;
        check_control(control)?;
        if size == 0 {
            break;
        }
        count += size as u64;
        ensure!(count <= limit, "snapshot exceeds byte limit");
        output.write_all(&buffer[..size])?;
        check_control(control)?;
        digest.update(&buffer[..size]);
    }
    unchanged(&before, &input.metadata()?)?;
    unchanged(&before, &fs::metadata(source)?)?;
    ensure!(count == before.len(), "snapshot copy length differs");
    check_control(control)?;
    Ok((digest.finalize().into(), count))
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(
        bytes[at..at + 4]
            .try_into()
            .expect("validated header bounds"),
    )
}

/// Validate the bounded database/WAL container without projecting catalogue rows.
pub fn validate_container(database: &Path, wal: Option<&Path>) -> Result<()> {
    validate_container_checked(database, wal, None)
}
/// Validate the same container with explicit cancellation and deadline checks.
pub fn validate_container_with_control(
    database: &Path,
    wal: Option<&Path>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let control = ReadControl {
        deadline,
        cancelled,
    };
    validate_container_checked(database, wal, Some(&control))
}
fn validate_container_checked(
    database: &Path,
    wal: Option<&Path>,
    control: Option<&ReadControl>,
) -> Result<()> {
    check_control(control)?;
    let mut db = File::open(database)?;
    let mut header = [0; 100];
    db.read_exact(&mut header)
        .context("truncated SQLite header")?;
    check_control(control)?;
    ensure!(
        &header[..16] == b"SQLite format 3\0",
        "invalid SQLite header"
    );
    let raw_page = u16::from_be_bytes([header[16], header[17]]);
    let page_size = if raw_page == 1 {
        65_536
    } else {
        usize::from(raw_page)
    };
    ensure!(
        (512..=65_536).contains(&page_size) && page_size.is_power_of_two(),
        "unsupported SQLite page size"
    );
    ensure!(
        db.metadata()?.len().is_multiple_of(page_size as u64),
        "partial SQLite database page"
    );
    ensure!(
        [1, 2].contains(&header[18]) && [1, 2].contains(&header[19]),
        "unsupported SQLite file version"
    );
    if let Some(path) = wal {
        let bytes = match control {
            None => fs::read(path)?,
            Some(_) => read_wal_checked(path, control)?,
        };
        check_control(control)?;
        if bytes.is_empty() {
            check_control(control)?;
            return Ok(());
        }
        ensure!(bytes.len() >= 32, "truncated WAL header");
        let magic = be32(&bytes, 0);
        ensure!(
            [0x377f0682, 0x377f0683].contains(&magic) && be32(&bytes, 4) == 3_007_000,
            "unsupported WAL header"
        );
        ensure!(
            be32(&bytes, 8) as usize == page_size && header[18] == 2 && header[19] == 2,
            "WAL/database mode or page-size mismatch"
        );
        // SQLite format4.1/4.2: rolling sums use the byte order selected by
        // magic bit0; stored checksums are always big-endian.
        let checksum = |data: &[u8], mut sums: [u32; 2]| {
            for chunk in data.chunks_exact(8) {
                let word = |v: &[u8]| {
                    if magic & 1 == 0 {
                        u32::from_le_bytes(v.try_into().expect("word"))
                    } else {
                        u32::from_be_bytes(v.try_into().expect("word"))
                    }
                };
                sums[0] = sums[0]
                    .wrapping_add(word(&chunk[..4]))
                    .wrapping_add(sums[1]);
                sums[1] = sums[1]
                    .wrapping_add(word(&chunk[4..]))
                    .wrapping_add(sums[0]);
            }
            sums
        };
        let mut sums = checksum(&bytes[..24], [0, 0]);
        ensure!(
            sums == [be32(&bytes, 24), be32(&bytes, 28)],
            "WAL header checksum differs"
        );
        let mut committed = bytes.len() == 32;
        let mut current_frames = 0usize;
        let mut previous_salts = None;
        for frame in bytes[32..].chunks(page_size + 24) {
            check_control(control)?;
            ensure!(frame.len() >= 24, "partial WAL frame header");
            ensure!(
                be32(frame, 0) != 0
                    && u64::from(be32(frame, 0)) * page_size as u64 <= MAX_DATABASE_BYTES,
                "WAL page exceeds database bound"
            );
            ensure!(
                u64::from(be32(frame, 4)) * page_size as u64 <= MAX_DATABASE_BYTES,
                "WAL commit exceeds database bound"
            );
            if frame[8..16] != bytes[16..24] {
                // SQLite resets salt1 by incrementing it and randomizes salt2.
                // A reset can leave complete old frames and a partial last one.
                // Accept only one coherent immediately preceding generation,
                // after a fully validated committed current prefix. Its payload
                // is not current data; the overwritten old prefix prevents us
                // from recomputing its rolling checksums. Keep the entire WAL
                // unchanged and hash-bound for SQLite's read-only recovery.
                ensure!(
                    current_frames > 0 && committed,
                    "WAL reset tail lacks a committed current prefix"
                );
                let salts = [be32(frame, 8), be32(frame, 12)];
                ensure!(
                    salts[0].wrapping_add(1) == be32(&bytes, 16) && salts[1] != be32(&bytes, 20),
                    "WAL tail is not the immediately preceding generation"
                );
                ensure!(
                    previous_salts.is_none_or(|previous| previous == salts),
                    "WAL reset tail has mixed generations"
                );
                previous_salts = Some(salts);
                continue;
            }
            ensure!(
                previous_salts.is_none(),
                "current WAL frame follows a reset tail"
            );
            ensure!(
                frame.len() == page_size + 24,
                "partial current-generation WAL frame"
            );
            sums = checksum(&frame[..8], sums);
            sums = checksum(&frame[24..], sums);
            ensure!(
                sums == [be32(frame, 16), be32(frame, 20)],
                "WAL frame checksum differs"
            );
            current_frames += 1;
            committed = be32(frame, 4) != 0;
        }
        ensure!(committed, "WAL ends in an uncommitted transaction");
    }
    check_control(control)?;
    Ok(())
}
fn read_wal_checked(path: &Path, control: Option<&ReadControl>) -> Result<Vec<u8>> {
    check_control(control)?;
    let (mut input, before) = source_file(path, MAX_WAL_BYTES)?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65_536];
    loop {
        check_control(control)?;
        let count = input.read(&mut buffer)?;
        check_control(control)?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len() as u64 + count as u64 <= MAX_WAL_BYTES,
            "WAL exceeds byte limit"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
    unchanged(&before, &input.metadata()?)?;
    unchanged(&before, &fs::metadata(path)?)?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "WAL read length differs"
    );
    check_control(control)?;
    Ok(bytes)
}

#[derive(Clone)]
struct ReadControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}
impl ReadControl {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            "catalog read cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "catalog read deadline exceeded"
        );
        Ok(())
    }
}
fn check_control(control: Option<&ReadControl>) -> Result<()> {
    if let Some(control) = control {
        control.check()?;
    }
    Ok(())
}
fn next_row<'rows, 'statement>(
    rows: &'rows mut rusqlite::Rows<'statement>,
    control: Option<&ReadControl>,
) -> Result<Option<&'rows Row<'statement>>> {
    check_control(control)?;
    let row = rows.next()?;
    check_control(control)?;
    Ok(row)
}

fn open_readonly_with_control(
    path: &Path,
    control: &ReadControl,
) -> Result<(Connection, ReadControl)> {
    open_readonly_with_control_options(path, control, false)
}
fn open_readonly_with_control_options(
    path: &Path,
    control: &ReadControl,
    memory_temp: bool,
) -> Result<(Connection, ReadControl)> {
    let phase = ReadControl {
        deadline: control
            .deadline
            .min(Instant::now() + Duration::from_secs(10)),
        cancelled: control.cancelled.clone(),
    };
    let connection = open_readonly_checked_options(path, Some(&phase), memory_temp)?;
    phase.check()?;
    Ok((connection, phase))
}

/// Switch to in-memory temporary storage and require SQLite to acknowledge it.
/// `requested` runs between the request and its acknowledgement queries.
fn memory_temp_storage(
    connection: &Connection,
    requested: impl FnOnce() -> Result<()>,
) -> Result<()> {
    connection.execute_batch("PRAGMA temp_store=MEMORY;")?;
    requested()?;
    let forced_disk: i64 = connection.query_row(
        "SELECT sqlite_compileoption_used('TEMP_STORE=0')",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        forced_disk == 0,
        "SQLite build forces disk temporary storage"
    );
    let actual: i64 = connection.query_row("PRAGMA temp_store", [], |row| row.get(0))?;
    ensure!(
        actual == 2,
        "SQLite memory temporary storage was not acknowledged"
    );
    Ok(())
}

fn open_readonly(path: &Path) -> Result<Connection> {
    open_readonly_checked_options(path, None, false)
}
fn open_readonly_checked_options(
    path: &Path,
    control: Option<&ReadControl>,
    memory_temp: bool,
) -> Result<Connection> {
    check_control(control)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    check_control(control)?;
    connection
        .execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0; PRAGMA cache_size=-8192;")?;
    check_control(control)?;
    if memory_temp {
        memory_temp_storage(&connection, || Ok(()))?;
        check_control(control)?;
    }
    connection.busy_timeout(Duration::from_millis(100))?;
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, 16 * 1024 * 1024),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, 1024 * 1024),
        (Limit::SQLITE_LIMIT_COLUMN, 512),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
    ] {
        connection.set_limit(limit, value)?;
    }
    let start = Instant::now();
    let controlled = control.cloned();
    let mut steps = 0u64;
    connection.progress_handler(
        10_000,
        Some(move || {
            steps += 10_000;
            steps > 100_000_000
                || start.elapsed() > Duration::from_secs(10)
                || controlled.as_ref().is_some_and(|control| {
                    control.cancelled.load(Ordering::Relaxed) || Instant::now() >= control.deadline
                })
        }),
    )?;
    connection.authorizer(Some(
        |context: rusqlite::hooks::AuthContext<'_>| match context.action {
            AuthAction::Read { .. } | AuthAction::Select | AuthAction::Transaction { .. } => {
                Authorization::Allow
            }
            AuthAction::Function { function_name }
                if ["count", "typeof"].contains(&function_name) =>
            {
                Authorization::Allow
            }
            AuthAction::Pragma { pragma_name, .. }
                if ["user_version", "integrity_check", "table_xinfo"].contains(&pragma_name) =>
            {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },
    ))?;
    check_control(control)?;
    Ok(connection)
}

type TableSpec = (
    &'static str,
    Option<&'static str>,
    &'static [(&'static str, &'static str)],
);
const TABLES: &[TableSpec] = &[
    (
        "item",
        Some("item_pid"),
        &[
            ("item_pid", "INTEGER"),
            ("media_type", "INTEGER"),
            ("album_pid", "INTEGER"),
            ("album_artist_pid", "INTEGER"),
            ("item_artist_pid", "INTEGER"),
            ("genre_id", "INTEGER"),
            ("composer_pid", "INTEGER"),
            ("base_location_id", "INTEGER"),
            ("remote_location_id", "INTEGER"),
            ("track_number", "INTEGER"),
            ("disc_number", "INTEGER"),
            ("is_compilation", "INTEGER"),
            ("composer_order", "INTEGER"),
        ],
    ),
    (
        "item_extra",
        Some("item_pid"),
        &[
            ("item_pid", "INTEGER"),
            ("title", "TEXT"),
            ("location", "TEXT"),
            ("total_time_ms", "REAL"),
            ("file_size", "INTEGER"),
            ("track_count", "INTEGER"),
            ("disc_count", "INTEGER"),
            ("year", "INTEGER"),
            ("copyright", "TEXT"),
            ("content_rating", "INTEGER"),
            ("content_rating_level", "INTEGER"),
            ("sort_title", "TEXT"),
            ("comment", "TEXT"),
        ],
    ),
    (
        "item_store",
        Some("item_pid"),
        &[("item_pid", "INTEGER"), ("sync_id", "INTEGER")],
    ),
    (
        "item_state",
        Some("item_pid"),
        &[
            ("item_pid", "INTEGER"),
            ("is_placeholder", "INTEGER"),
            ("is_library_added", "INTEGER"),
            ("has_non_purgeable_asset", "INTEGER"),
            ("is_supported_type", "INTEGER"),
        ],
    ),
    (
        "item_stats",
        Some("item_pid"),
        &[
            ("item_pid", "INTEGER"),
            ("play_count_user", "INTEGER"),
            ("play_count_recent", "INTEGER"),
            ("skip_count_user", "INTEGER"),
            ("skip_count_recent", "INTEGER"),
            ("has_been_played", "INTEGER"),
            ("user_rating", "INTEGER"),
            ("user_rating_is_derived", "INTEGER"),
            ("liked_state", "INTEGER"),
        ],
    ),
    (
        "album",
        Some("album_pid"),
        &[
            ("album_pid", "INTEGER"),
            ("sync_id", "INTEGER"),
            ("album", "TEXT"),
            ("sort_album", "TEXT"),
            ("album_artist_pid", "INTEGER"),
            ("all_compilations", "INTEGER"),
        ],
    ),
    (
        "album_artist",
        Some("album_artist_pid"),
        &[
            ("album_artist_pid", "INTEGER"),
            ("sync_id", "INTEGER"),
            ("album_artist", "TEXT"),
            ("sort_album_artist", "TEXT"),
        ],
    ),
    (
        "item_artist",
        Some("item_artist_pid"),
        &[
            ("item_artist_pid", "INTEGER"),
            ("sync_id", "INTEGER"),
            ("item_artist", "TEXT"),
            ("sort_item_artist", "TEXT"),
        ],
    ),
    (
        "base_location",
        Some("base_location_id"),
        &[("base_location_id", "INTEGER"), ("path", "TEXT")],
    ),
    (
        "genre",
        Some("genre_id"),
        &[
            ("genre_id", "INTEGER"),
            ("sync_id", "INTEGER"),
            ("genre", "TEXT"),
        ],
    ),
    (
        "composer",
        Some("composer_pid"),
        &[
            ("composer_pid", "INTEGER"),
            ("sync_id", "INTEGER"),
            ("composer", "TEXT"),
            ("sort_composer", "TEXT"),
        ],
    ),
    (
        "container",
        Some("container_pid"),
        &[("container_pid", "INTEGER"), ("sync_id", "INTEGER")],
    ),
    (
        "db_info",
        Some("db_pid"),
        &[("db_pid", "INTEGER"), ("primary_container_pid", "INTEGER")],
    ),
    (
        "_MLDatabaseProperties",
        Some("key"),
        &[("key", "TEXT"), ("value", "TEXT")],
    ),
    (
        "artwork",
        None,
        &[
            ("artwork_token", "TEXT"),
            ("artwork_source_type", "INTEGER"),
            ("artwork_type", "INTEGER"),
            ("artwork_variant_type", "INTEGER"),
            ("relative_path", "TEXT"),
        ],
    ),
    (
        "artwork_token",
        None,
        &[
            ("entity_pid", "INTEGER"),
            ("artwork_token", "TEXT"),
            ("artwork_source_type", "INTEGER"),
            ("artwork_type", "INTEGER"),
            ("entity_type", "INTEGER"),
            ("artwork_variant_type", "INTEGER"),
        ],
    ),
];

/// Returns the user_version and each `TABLES` row count.
fn validate_schema(
    connection: &Connection,
    control: Option<&ReadControl>,
) -> Result<(i64, BTreeMap<&'static str, usize>)> {
    check_control(control)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    check_control(control)?;
    ensure!(
        version >= USER_VERSION_FLOOR,
        "unsupported MediaLibrary schema version {version}, older than {USER_VERSION_FLOOR}"
    );
    check_control(control)?;
    let schema_count: i64 =
        connection.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    check_control(control)?;
    ensure!(
        (0..=4096).contains(&schema_count),
        "SQLite schema exceeds bound"
    );
    check_control(control)?;
    let mut integrity = connection.prepare("PRAGMA integrity_check(1)")?;
    check_control(control)?;
    let mut result = integrity.query([])?;
    check_control(control)?;
    ensure!(
        next_row(&mut result, control)?
            .context("integrity check empty")?
            .get::<_, String>(0)?
            == "ok"
            && next_row(&mut result, control)?.is_none(),
        "SQLite integrity check failed"
    );
    let mut counts = BTreeMap::new();
    let mut total = 0usize;
    for &(table, primary, columns) in TABLES {
        check_control(control)?;
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .context("required catalog table absent")?;
        check_control(control)?;
        ensure!(
            sql.trim_start()
                .to_ascii_uppercase()
                .starts_with("CREATE TABLE "),
            "catalog table is not an ordinary table"
        );
        check_control(control)?;
        let mut statement = connection.prepare(&format!("PRAGMA table_xinfo({table})"))?;
        check_control(control)?;
        let mut rows = statement.query([])?;
        check_control(control)?;
        let mut found = BTreeMap::new();
        let mut primary_columns = Vec::new();
        while let Some(row) = next_row(&mut rows, control)? {
            ensure!(found.len() < 512, "catalog table has too many columns");
            let name: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let pk: i64 = row.get(5)?;
            let hidden: i64 = row.get(6)?;
            if pk != 0 {
                primary_columns.push(name.clone());
            }
            ensure!(hidden == 0, "generated/hidden catalog columns unsupported");
            found.insert(name, kind);
        }
        ensure!(
            primary_columns
                .iter()
                .map(String::as_str)
                .eq(primary.into_iter()),
            "catalog table primary key differs"
        );
        for &(name, kind) in columns {
            check_control(control)?;
            ensure!(
                found
                    .get(name)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(kind)),
                "required catalog column/type differs"
            );
        }
        check_control(control)?;
        let count: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        check_control(control)?;
        let count = usize::try_from(count)?;
        ensure!(count <= MAX_ROWS, "catalog table row limit exceeded");
        total += count;
        ensure!(
            total <= MAX_TOTAL_ROWS,
            "aggregate catalog row limit exceeded"
        );
        check_control(control)?;
        counts.insert(table, count);
    }
    check_control(control)?;
    Ok((version, counts))
}

struct TextBudget(usize);
impl TextBudget {
    fn cloned(&mut self, value: &str) -> Result<String> {
        self.0 += value.len();
        ensure!(
            self.0 <= MAX_TOTAL_TEXT_BYTES,
            "aggregate catalog text exceeds bound"
        );
        Ok(value.to_owned())
    }
    fn optional(&mut self, row: &Row<'_>, name: &str) -> Result<Option<String>> {
        match row.get_ref(name)? {
            ValueRef::Null => Ok(None),
            ValueRef::Text(bytes) => {
                ensure!(
                    bytes.len() <= MAX_TEXT_BYTES,
                    "catalog text field exceeds bound"
                );
                self.0 += bytes.len();
                ensure!(
                    self.0 <= MAX_TOTAL_TEXT_BYTES,
                    "aggregate catalog text exceeds bound"
                );
                let text = std::str::from_utf8(bytes).context("catalog text is not UTF-8")?;
                ensure!(!text.contains('\0'), "catalog text contains NUL");
                Ok(Some(text.to_owned()))
            }
            _ => bail!("catalog text field has wrong storage type"),
        }
    }
    fn required(&mut self, row: &Row<'_>, name: &str) -> Result<String> {
        self.optional(row, name)?
            .context("required catalog text is absent")
    }
}
fn integer(row: &Row<'_>, name: &str) -> Result<i64> {
    match row.get_ref(name)? {
        ValueRef::Integer(value) => Ok(value),
        _ => bail!("catalog integer field has wrong storage type"),
    }
}
fn wire_id(row: &Row<'_>, name: &str) -> Result<u64> {
    let value = integer(row, name)? as u64;
    ensure!(value != 0, "required catalog sync identity is zero");
    Ok(value)
}
/// An item no sync made (`item_store.sync_id` 0), which the phone adds itself
/// for an entry outside the library. Observed: an Apple Music track it
/// streamed, with a remote location, no file, and an album and album artist
/// without sync identities. No operation can name one, so the catalogue leaves
/// it out. One that holds a file is refused: new tracks are named against the
/// catalogue's paths, and its path would not be among them.
fn unsynced_item(row: &Row<'_>) -> Result<i64> {
    let internal_id = integer(row, "item_pid")?;
    ensure!(internal_id != 0, "required track join identity is zero");
    ensure!(
        integer(row, "base_location_id")? == 0
            && matches!(
                row.get_ref("location")?,
                ValueRef::Null | ValueRef::Text(b"")
            )
            && !flag(row, "has_non_purgeable_asset")?,
        "an item no sync made holds a file on the phone"
    );
    Ok(internal_id)
}
fn number(row: &Row<'_>, name: &str) -> Result<u32> {
    u32::try_from(integer(row, name)?).context("catalog number outside u32 range")
}
fn flag(row: &Row<'_>, name: &str) -> Result<bool> {
    let value = integer(row, name)?;
    ensure!([0, 1].contains(&value), "catalog boolean outside range");
    Ok(value == 1)
}
fn duration(row: &Row<'_>) -> Result<u32> {
    match row.get_ref("total_time_ms")? {
        ValueRef::Integer(value) => u32::try_from(value).context("catalog duration outside range"),
        ValueRef::Real(value) => {
            ensure!(
                value.is_finite()
                    && value >= 0.0
                    && value <= f64::from(u32::MAX)
                    && value.fract() == 0.0,
                "catalog duration is not bounded whole milliseconds"
            );
            Ok(value as u32)
        }
        _ => bail!("catalog duration has wrong storage type"),
    }
}

fn music_path(base: &str, leaf: &str) -> Result<String> {
    let parts: Vec<_> = base.split('/').collect();
    ensure!(
        parts.len() == 3 && parts[0] == "iTunes_Control" && parts[1] == "Music",
        "unsupported catalog media root"
    );
    let bucket = parts[2].as_bytes();
    ensure!(
        bucket.len() == 3 && bucket[0] == b'F' && bucket[1..].iter().all(u8::is_ascii_digit),
        "unsupported Music bucket"
    );
    ensure!(
        !leaf.is_empty() && leaf.len() <= 255 && leaf != "." && leaf != "..",
        "invalid Music filename"
    );
    ensure!(
        leaf.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "unsafe Music filename"
    );
    Ok(format!("{base}/{leaf}"))
}

fn parse_catalog(
    connection: &Connection,
    source_hashes: SourceHashes,
    control: Option<&ReadControl>,
) -> Result<Catalog> {
    check_control(control)?;
    connection.execute_batch("BEGIN DEFERRED")?;
    check_control(control)?;
    let (user_version, counts) = validate_schema(connection, control)?;
    ensure!(
        counts["db_info"] == 1,
        "catalog db_info cardinality differs"
    );
    for table in ["item_extra", "item_store", "item_state", "item_stats"] {
        check_control(control)?;
        ensure!(
            counts[table] == counts["item"],
            "catalog item companion cardinality differs"
        );
        let orphans: i64 = connection.query_row(&format!("SELECT count(*) FROM {table} t LEFT JOIN item i USING(item_pid) WHERE i.item_pid IS NULL"), [], |r| r.get(0))?;
        check_control(control)?;
        ensure!(orphans == 0, "catalog item companion has orphan rows");
    }
    let mut text = TextBudget(0);
    check_control(control)?;
    let mut statement = connection.prepare("SELECT db_pid,primary_container_pid FROM db_info")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    let row = next_row(&mut rows, control)?.context("catalog db_info absent")?;
    let database_pid = wire_id(row, "db_pid")?;
    let primary_container_pid = wire_id(row, "primary_container_pid")?;
    check_control(control)?;
    let anchor_text: String = connection
        .query_row(
            "SELECT value FROM _MLDatabaseProperties WHERE key='MLSyncClientSyncAnchor'",
            [],
            |r| r.get(0),
        )
        .context("persisted Media anchor absent")?;
    check_control(control)?;
    ensure!(
        !anchor_text.is_empty()
            && anchor_text.len() <= 10
            && anchor_text.bytes().all(|b| b.is_ascii_digit()),
        "persisted Media anchor is not canonical unsigned decimal"
    );
    let anchor: u32 = anchor_text
        .parse()
        .context("persisted Media anchor outside range")?;
    ensure!(
        anchor.to_string() == anchor_text,
        "persisted Media anchor has noncanonical spelling"
    );
    // The phone makes groups of its own, with sync_id 0, for the entries it adds
    // outside the library (observed: an Apple Music track it streamed). They are
    // kept apart, and a track of the library may not join one.
    let mut artists = BTreeMap::new();
    let mut unsynced_artists = BTreeMap::new();
    let mut artist_sync_ids = BTreeSet::new();
    check_control(control)?;
    let mut statement = connection.prepare(
        "SELECT album_artist_pid,sync_id,album_artist,sort_album_artist FROM album_artist",
    )?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    while let Some(row) = next_row(&mut rows, control)? {
        let internal_id = integer(row, "album_artist_pid")?;
        let sync_id = integer(row, "sync_id")? as u64;
        ensure!(
            internal_id != 0 && (sync_id == 0 || artist_sync_ids.insert(sync_id)),
            "ambiguous catalog artist identity"
        );
        if sync_id == 0 {
            &mut unsynced_artists
        } else {
            &mut artists
        }
        .insert(
            internal_id,
            Artist {
                internal_id,
                sync_id,
                name: text.required(row, "album_artist")?,
                sort_name: text.optional(row, "sort_album_artist")?,
                live_track_count: 0,
            },
        );
    }
    let mut albums = BTreeMap::new();
    let mut unsynced_albums = BTreeMap::new();
    let mut album_sync_ids = BTreeSet::new();
    check_control(control)?;
    let mut statement = connection.prepare(
        "SELECT album_pid,sync_id,album,sort_album,album_artist_pid,all_compilations FROM album",
    )?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    while let Some(row) = next_row(&mut rows, control)? {
        let internal_id = integer(row, "album_pid")?;
        let sync_id = integer(row, "sync_id")? as u64;
        let artist_internal_id = integer(row, "album_artist_pid")?;
        // An album with a sync identity must be under an artist with one.
        let artist_sync_id = artists
            .get(&artist_internal_id)
            .or_else(|| {
                (sync_id == 0)
                    .then(|| unsynced_artists.get(&artist_internal_id))
                    .flatten()
            })
            .context("album artist join absent")?
            .sync_id;
        ensure!(
            internal_id != 0 && (sync_id == 0 || album_sync_ids.insert(sync_id)),
            "ambiguous catalog album identity"
        );
        if sync_id == 0 {
            &mut unsynced_albums
        } else {
            &mut albums
        }
        .insert(
            internal_id,
            Album {
                internal_id,
                sync_id,
                name: text.required(row, "album")?,
                sort_name: text.optional(row, "sort_album")?,
                artist_internal_id,
                artist_sync_id,
                live_track_count: 0,
                all_compilations: flag(row, "all_compilations")?,
                has_compilation_tracks: false,
            },
        );
    }
    let mut composers = BTreeMap::new();
    check_control(control)?;
    let mut statement =
        connection.prepare("SELECT composer_pid,sync_id,composer,sort_composer FROM composer")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    while let Some(row) = next_row(&mut rows, control)? {
        ensure!(composers.len() < MAX_ROWS, "composer row bound exceeded");
        let internal_id = integer(row, "composer_pid")?;
        ensure!(internal_id != 0, "zero composer lookup identity");
        let value = ComposerLookup {
            internal_id,
            sync_id: integer(row, "sync_id")? as u64,
            name: text.required(row, "composer")?,
            sort_name: text.optional(row, "sort_composer")?,
        };
        ensure!(
            composers.insert(internal_id, value).is_none(),
            "duplicate composer lookup"
        );
    }
    let mut item_artists = BTreeMap::new();
    check_control(control)?;
    let mut statement = connection
        .prepare("SELECT item_artist_pid,item_artist,sort_item_artist FROM item_artist")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    while let Some(row) = next_row(&mut rows, control)? {
        ensure!(
            item_artists.len() < MAX_ROWS,
            "item artist row bound exceeded"
        );
        let internal_id = integer(row, "item_artist_pid")?;
        let value = ItemArtist {
            internal_id,
            name: text.optional(row, "item_artist")?.unwrap_or_default(),
            sort_name: text.optional(row, "sort_item_artist")?,
        };
        ensure!(
            item_artists.insert(internal_id, value).is_none(),
            "duplicate item artist row"
        );
    }
    let mut represented_sync_ids = BTreeSet::from([database_pid, primary_container_pid]);
    for table in [
        "item_store",
        "album",
        "album_artist",
        "item_artist",
        "composer",
        "genre",
        "container",
    ] {
        check_control(control)?;
        let mut statement = connection.prepare(&format!("SELECT sync_id FROM {table}"))?;
        check_control(control)?;
        let mut rows = statement.query([])?;
        check_control(control)?;
        while let Some(row) = next_row(&mut rows, control)? {
            let id = integer(row, "sync_id")? as u64;
            if id != 0 {
                represented_sync_ids.insert(id);
            }
        }
    }
    let mut tracks = Vec::with_capacity(counts["item"]);
    let mut seen = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut track_composers = BTreeMap::new();
    let mut title_sorts = BTreeMap::new();
    let mut comments = BTreeMap::new();
    let mut track_item_artists = BTreeMap::new();
    let mut unsynced_items = BTreeSet::new();
    check_control(control)?;
    let mut statement = connection.prepare("SELECT i.item_pid,i.media_type,i.album_pid,i.album_artist_pid,i.item_artist_pid,i.genre_id,i.composer_pid,i.base_location_id,i.remote_location_id,i.track_number,i.disc_number,i.is_compilation,i.composer_order,s.sync_id,e.title,e.sort_title,e.comment,e.location,e.total_time_ms,e.file_size,e.track_count,e.disc_count,e.year,e.copyright,e.content_rating,e.content_rating_level,ia.item_artist AS artist,b.path AS base_path,g.genre,c.composer,st.is_placeholder,st.is_library_added,st.has_non_purgeable_asset,st.is_supported_type,ps.play_count_user,ps.play_count_recent,ps.skip_count_user,ps.skip_count_recent,ps.has_been_played,ps.user_rating,ps.user_rating_is_derived,ps.liked_state FROM item i JOIN item_store s USING(item_pid) JOIN item_extra e USING(item_pid) JOIN item_state st USING(item_pid) JOIN item_stats ps USING(item_pid) LEFT JOIN item_artist ia USING(item_artist_pid) LEFT JOIN base_location b USING(base_location_id) LEFT JOIN genre g USING(genre_id) LEFT JOIN composer c USING(composer_pid) ORDER BY s.sync_id")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    while let Some(row) = next_row(&mut rows, control)? {
        ensure!(
            tracks.len() + unsynced_items.len() < MAX_ROWS,
            "track row bound exceeded"
        );
        let sync_id = integer(row, "sync_id")? as u64;
        if sync_id == 0 {
            unsynced_items.insert(unsynced_item(row)?);
            continue;
        }
        ensure!(seen.insert(sync_id), "duplicate track sync identity");
        title_sorts.insert(sync_id, text.optional(row, "sort_title")?);
        comments.insert(sync_id, text.optional(row, "comment")?);
        track_item_artists.insert(sync_id, integer(row, "item_artist_pid")?);
        ensure!(
            integer(row, "media_type")? == 8 && integer(row, "remote_location_id")? == 0,
            "unsupported nonlocal or nonmusic catalog item"
        );
        let internal_id = integer(row, "item_pid")?;
        ensure!(
            internal_id != 0 && integer(row, "item_artist_pid")? != 0,
            "required track join identity is zero"
        );
        let album = albums
            .get_mut(&integer(row, "album_pid")?)
            .context("track album join absent")?;
        let artist = artists
            .get_mut(&integer(row, "album_artist_pid")?)
            .context("track artist join absent")?;
        ensure!(
            album.artist_internal_id == artist.internal_id,
            "track and album artist joins conflict"
        );
        album.live_track_count += 1;
        let is_compilation = flag(row, "is_compilation")?;
        album.has_compilation_tracks |= is_compilation;
        artist.live_track_count += 1;
        let genre = text.optional(row, "genre")?;
        let composer = text.optional(row, "composer")?;
        ensure!(
            integer(row, "genre_id")? == 0 || genre.is_some(),
            "track genre join absent"
        );
        ensure!(
            integer(row, "composer_pid")? == 0 || composer.is_some(),
            "track composer join absent"
        );
        let composer_id = integer(row, "composer_pid")?;
        let lookup_id = (composer_id != 0).then_some(composer_id);
        ensure!(
            lookup_id
                .and_then(|id| composers.get(&id))
                .map(|row| row.name.as_str())
                == composer.as_deref(),
            "track composer lookup disagrees with joined label"
        );
        track_composers.insert(
            sync_id,
            ComposerReference {
                lookup_id,
                order: integer(row, "composer_order")?,
            },
        );
        let leaf = text.required(row, "location")?;
        let base = text.optional(row, "base_path")?;
        let base_id = integer(row, "base_location_id")?;
        // Apple retains base_location(0, "") as the unavailable-location sentinel.
        // Preserve such a track with no path; a nonempty base or leaf still fails
        // closed, and the installed-asset flag below still requires a real path.
        let afc_path =
            if base_id == 0 && leaf.is_empty() && (base.is_none() || base.as_deref() == Some("")) {
                None
            } else {
                ensure!(base_id != 0, "track media location join is malformed");
                Some(music_path(
                    &base.context("track media location join absent")?,
                    &leaf,
                )?)
            };
        if let Some(path) = &afc_path {
            ensure!(
                paths.insert(path.to_ascii_lowercase()),
                "catalog media paths alias"
            );
        }
        let availability = Availability {
            is_placeholder: flag(row, "is_placeholder")?,
            is_library_added: flag(row, "is_library_added")?,
            has_non_purgeable_asset: flag(row, "has_non_purgeable_asset")?,
            is_supported_type: flag(row, "is_supported_type")?,
        };
        ensure!(
            !availability.has_non_purgeable_asset || afc_path.is_some(),
            "catalog asset flag has no location"
        );
        tracks.push(Track {
            sync_id,
            internal_id,
            album_sync_id: album.sync_id,
            artist_sync_id: artist.sync_id,
            metadata: TrackMetadata {
                title: text.required(row, "title")?,
                artist: text.required(row, "artist")?,
                album: text.cloned(&album.name)?,
                album_artist: text.cloned(&artist.name)?,
                genre,
                composer,
                copyright: text.optional(row, "copyright")?,
                content_rating: number(row, "content_rating")?,
                content_rating_level: number(row, "content_rating_level")?,
                is_compilation,
                track_number: number(row, "track_number")?,
                track_count: number(row, "track_count")?,
                disc_number: number(row, "disc_number")?,
                disc_count: number(row, "disc_count")?,
                year: number(row, "year")?,
                total_time_ms: duration(row)?,
                file_size: u64::try_from(integer(row, "file_size")?)
                    .context("catalog file size is negative")?,
            },
            afc_path,
            availability,
            playback: PlaybackStatistics {
                play_count_user: number(row, "play_count_user")?,
                play_count_recent: number(row, "play_count_recent")?,
                skip_count_user: number(row, "skip_count_user")?,
                skip_count_recent: number(row, "skip_count_recent")?,
                has_been_played: flag(row, "has_been_played")?,
                user_rating: number(row, "user_rating")?,
                user_rating_is_derived: flag(row, "user_rating_is_derived")?,
                liked_state: number(row, "liked_state")?,
            },
        });
    }
    ensure!(
        tracks.len() + unsynced_items.len() == counts["item"],
        "track query omitted catalog rows"
    );
    let music_folders = connection
        .prepare("SELECT path FROM base_location")?
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        // A sentinel row may have no path at all.
        .flatten()
        .filter(|path| {
            path.strip_prefix("iTunes_Control/Music/F")
                .is_some_and(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    let artwork = parse_artwork(
        connection,
        &tracks,
        &albums,
        &unsynced_items,
        &unsynced_albums,
        control,
    )?;
    let stats_except_recent_play = stats_preservation(connection, &seen, control)?;
    let stats_except_recent_play_and_skip =
        stats_preservation_profile(connection, &seen, true, control)?;
    let (stats_except_playback, playback_dates) =
        stats_except_playback(connection, &seen, control)?;
    check_control(control)?;
    connection.execute_batch("ROLLBACK")?;
    check_control(control)?;
    Ok(Catalog {
        source_hashes,
        user_version,
        database_pid,
        primary_container_pid,
        anchor,
        tracks,
        albums,
        artists,
        unsynced_albums,
        unsynced_artists,
        composers,
        track_composers,
        title_sorts,
        comments,
        item_artists,
        track_item_artists,
        stats_except_recent_play,
        stats_except_recent_play_and_skip,
        stats_except_playback,
        playback_dates,
        represented_sync_ids,
        music_folders,
        artwork,
    })
}

fn stats_preservation(
    connection: &Connection,
    expected_ids: &BTreeSet<u64>,
    control: Option<&ReadControl>,
) -> Result<BTreeMap<u64, [u8; 32]>> {
    stats_preservation_profile(connection, expected_ids, false, control)
}

fn stats_preservation_profile(
    connection: &Connection,
    expected_ids: &BTreeSet<u64>,
    include_recent_skip: bool,
    control: Option<&ReadControl>,
) -> Result<BTreeMap<u64, [u8; 32]>> {
    let (domain, excluded): (&[u8], &[&str]) = if include_recent_skip {
        (
            b"music-protocol/item-stats-except-recent-play-and-skip\0",
            &["play_count_recent", "skip_count_recent"],
        )
    } else {
        (
            b"music-protocol/item-stats-except-play-count-recent\0",
            &["play_count_recent"],
        )
    };
    Ok(stats_fingerprints(connection, expected_ids, domain, excluded, control)?.0)
}

fn stats_except_playback(
    connection: &Connection,
    expected_ids: &BTreeSet<u64>,
    control: Option<&ReadControl>,
) -> Result<(BTreeMap<u64, [u8; 32]>, BTreeMap<u64, PlaybackDates>)> {
    stats_fingerprints(
        connection,
        expected_ids,
        b"music-protocol/item-stats-except-playback\0",
        &PLAYBACK_COLUMNS,
        control,
    )
}

/// Per-track fingerprints of every item_stats column except `excluded`, and
/// each track's playback dates.
fn stats_fingerprints(
    connection: &Connection,
    expected_ids: &BTreeSet<u64>,
    domain: &[u8],
    excluded: &[&str],
    control: Option<&ReadControl>,
) -> Result<(BTreeMap<u64, [u8; 32]>, BTreeMap<u64, PlaybackDates>)> {
    check_control(control)?;
    let mut query = connection.prepare(
        "SELECT s.sync_id AS __wire_id,ps.* FROM item_stats ps JOIN item_store s USING(item_pid) WHERE s.sync_id<>0 ORDER BY s.sync_id",
    )?;
    let names: Vec<String> = query
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    ensure!(names.len() <= 256, "statistics column bound exceeded");
    check_control(control)?;
    let mut rows = query.query([])?;
    check_control(control)?;
    let mut result = BTreeMap::new();
    let mut dates = BTreeMap::new();
    let mut variable_bytes = 0usize;
    while let Some(row) = next_row(&mut rows, control)? {
        ensure!(result.len() < MAX_ROWS, "statistics row bound exceeded");
        let id = wire_id(row, "__wire_id")?;
        let mut digest = Sha256::new();
        digest.update(domain);
        let mut date = PlaybackDates::default();
        for (index, name) in names.iter().enumerate().skip(1) {
            check_control(control)?;
            if let Some(slot) = match name.as_str() {
                "date_played" => Some(&mut date.played),
                "date_skipped" => Some(&mut date.skipped),
                _ => None,
            } {
                *slot = match row.get_ref(index)? {
                    ValueRef::Null => 0.0,
                    ValueRef::Integer(value) => value as f64,
                    ValueRef::Real(value) => value,
                    // Only a later-run check reads these; it refuses NaN.
                    ValueRef::Text(_) | ValueRef::Blob(_) => f64::NAN,
                };
            }
            if excluded.contains(&name.as_str()) {
                continue;
            }
            digest.update((name.len() as u64).to_le_bytes());
            digest.update(name.as_bytes());
            match row.get_ref(index)? {
                ValueRef::Null => digest.update([0]),
                ValueRef::Integer(value) => {
                    digest.update([1]);
                    digest.update(value.to_le_bytes());
                }
                ValueRef::Real(value) => {
                    ensure!(value.is_finite(), "nonfinite statistics value");
                    digest.update([2]);
                    digest.update(value.to_bits().to_le_bytes());
                }
                value @ (ValueRef::Text(_) | ValueRef::Blob(_)) => {
                    let (tag, bytes) = match value {
                        ValueRef::Text(bytes) => (3, bytes),
                        ValueRef::Blob(bytes) => (4, bytes),
                        _ => unreachable!(),
                    };
                    variable_bytes = variable_bytes
                        .checked_add(bytes.len())
                        .context("statistics byte count overflow")?;
                    ensure!(
                        bytes.len() <= MAX_TEXT_BYTES && variable_bytes <= MAX_TOTAL_TEXT_BYTES,
                        "statistics byte bound exceeded"
                    );
                    digest.update([tag]);
                    digest.update((bytes.len() as u64).to_le_bytes());
                    digest.update(bytes);
                }
            }
        }
        ensure!(
            result.insert(id, digest.finalize().into()).is_none(),
            "duplicate statistics identity"
        );
        dates.insert(id, date);
    }
    ensure!(
        result.keys().copied().collect::<BTreeSet<_>>() == *expected_ids,
        "statistics fingerprints do not cover every track"
    );
    check_control(control)?;
    Ok((result, dates))
}

// Captures from the phone pin this exact scoped decimal mapping against every
// baseline cache field and a traced artwork update. Source500 is a different
// namespace.
fn cache_id(row: &Row<'_>) -> Result<u32> {
    let ValueRef::Text(bytes) = row.get_ref("artwork_token")? else {
        bail!("source300 artwork token has nontext storage");
    };
    ensure!(
        !bytes.is_empty() && bytes.len() <= 10 && bytes.iter().all(u8::is_ascii_digit),
        "source300 artwork token is not bounded unsigned decimal"
    );
    let token = std::str::from_utf8(bytes)?;
    let id: u32 = token
        .parse()
        .context("source300 artwork token exceeds u32")?;
    ensure!(
        id >= 100 && id.to_string() == token,
        "source300 artwork token is reserved or noncanonical"
    );
    Ok(id)
}

/// Registrations of items and albums no sync made are counted, and their
/// source300 IDs held, but they are keyed to nothing.
fn parse_artwork(
    connection: &Connection,
    tracks: &[Track],
    albums: &BTreeMap<i64, Album>,
    unsynced_items: &BTreeSet<i64>,
    unsynced_albums: &BTreeMap<i64, Album>,
    control: Option<&ReadControl>,
) -> Result<ArtworkInventory> {
    check_control(control)?;
    let mut inventory = ArtworkInventory {
        occupied_cache_ids: BTreeSet::new(),
        cached_ids: BTreeSet::new(),
        cached_objects: BTreeMap::new(),
        track_cache_ids: BTreeMap::new(),
        tracks_with_artwork_registration: BTreeSet::new(),
        album_cache_ids: BTreeMap::new(),
        source300_rows: ArtworkRowCounts::default(),
        other_source_rows: BTreeMap::new(),
        unmapped_source300_token_rows: 0,
        orphan_album_cache_ids: BTreeMap::new(),
    };
    let track_ids: BTreeMap<_, _> = tracks
        .iter()
        .map(|track| (track.internal_id, track.sync_id))
        .collect();
    check_control(control)?;
    let mut statement =
        connection.prepare("SELECT artwork_token,artwork_source_type,artwork_type,artwork_variant_type,relative_path FROM artwork")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    let mut seen = 0;
    while let Some(row) = next_row(&mut rows, control)? {
        seen += 1;
        ensure!(seen <= MAX_ROWS, "artwork row bound exceeded");
        let source = integer(row, "artwork_source_type")?;
        if source != 300 {
            inventory
                .other_source_rows
                .entry(source)
                .or_default()
                .cached_rows += 1;
            continue;
        }
        let id = cache_id(row)?;
        let variant = integer(row, "artwork_variant_type")?;
        let artwork_type = integer(row, "artwork_type")?;
        let relative_path = match row.get_ref("relative_path")? {
            ValueRef::Text(bytes) if bytes.len() == 41 => {
                std::str::from_utf8(bytes).context("invalid cached artwork path UTF-8")?
            }
            _ => bail!("cached artwork path must be bounded canonical text"),
        };
        ensure!(
            relative_path.as_bytes()[2] == b'/'
                && relative_path
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| index == 2
                        || byte.is_ascii_digit()
                        || (b'a'..=b'f').contains(&byte)),
            "unsupported cached artwork relative path"
        );
        ensure!(
            inventory
                .cached_objects
                .insert(
                    (id, variant),
                    CachedArtwork {
                        cache_id: id,
                        artwork_type,
                        variant,
                        relative_path: relative_path.to_owned(),
                    }
                )
                .is_none(),
            "duplicate source300 cached artwork object"
        );
        inventory.source300_rows.cached_rows += 1;
        inventory.cached_ids.insert(id);
        inventory.occupied_cache_ids.insert(id);
    }
    check_control(control)?;
    let mut statement = connection.prepare("SELECT entity_pid,artwork_token,artwork_source_type,artwork_type,entity_type,artwork_variant_type FROM artwork_token")?;
    check_control(control)?;
    let mut rows = statement.query([])?;
    check_control(control)?;
    let mut seen = 0;
    while let Some(row) = next_row(&mut rows, control)? {
        seen += 1;
        ensure!(seen <= MAX_ROWS, "artwork relation row bound exceeded");
        let source = integer(row, "artwork_source_type")?;
        if row.get_ref("entity_type")? == ValueRef::Integer(0) {
            let entity = integer(row, "entity_pid")?;
            if !unsynced_items.contains(&entity) {
                let sync_id = *track_ids
                    .get(&entity)
                    .context("track artwork registration join absent")?;
                inventory.tracks_with_artwork_registration.insert(sync_id);
            }
        }
        if source != 300 {
            inventory
                .other_source_rows
                .entry(source)
                .or_default()
                .registration_rows += 1;
            continue;
        }
        let id = cache_id(row)?;
        inventory.source300_rows.registration_rows += 1;
        inventory.occupied_cache_ids.insert(id);
        let shape = (
            integer(row, "entity_type")?,
            integer(row, "artwork_type")?,
            integer(row, "artwork_variant_type")?,
        );
        match shape {
            (0, 1, 0) => {
                let entity = integer(row, "entity_pid")?;
                if unsynced_items.contains(&entity) {
                    inventory.unmapped_source300_token_rows += 1;
                    continue;
                }
                let sync_id = *track_ids
                    .get(&entity)
                    .context("source300 track artwork join absent")?;
                ensure!(
                    inventory.track_cache_ids.insert(sync_id, id).is_none(),
                    "duplicate source300 track artwork relationship"
                );
            }
            (4, 6, 0) => {
                let entity = integer(row, "entity_pid")?;
                if unsynced_albums.contains_key(&entity) {
                    inventory.unmapped_source300_token_rows += 1;
                    continue;
                }
                let Some(album) = albums.get(&entity) else {
                    ensure!(
                        inventory
                            .orphan_album_cache_ids
                            .insert(entity, id)
                            .is_none(),
                        "duplicate source300 orphan album artwork relationship"
                    );
                    continue;
                };
                let sync_id = album.sync_id;
                ensure!(
                    inventory.album_cache_ids.insert(sync_id, id).is_none(),
                    "duplicate source300 album artwork relationship"
                );
            }
            _ => inventory.unmapped_source300_token_rows += 1,
        }
    }
    check_control(control)?;
    Ok(inventory)
}

#[cfg(test)]
mod current_storage_profile_tests {
    use super::*;
    #[test]
    fn controlled_real_sqlite_uses_memory_temp_and_preserves_source_bytes() {
        let path = std::env::temp_dir().join(format!(
            "current-sqlite-memory-{}-{}",
            std::process::id(),
            SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let db = path.join("small.db");
        {
            let connection = Connection::open(&db).unwrap();
            connection
                .execute_batch("CREATE TABLE t(n INTEGER); INSERT INTO t VALUES(3),(1),(2);")
                .unwrap();
        }
        let before = fs::read(&db).unwrap();
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let connection = open_readonly_checked_options(&db, Some(&control), true).unwrap();
        // An allowed production read exercises the actual configured connection.
        let mut rows = connection.prepare("SELECT n FROM t ORDER BY n").unwrap();
        let values = rows
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(values, vec![1, 2, 3]);
        drop(rows);
        drop(connection);
        assert_eq!(fs::read(&db).unwrap(), before);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        fs::remove_file(db).unwrap();
        fs::remove_dir(path).unwrap();
    }
    #[test]
    fn current_sqlite_cancellation_is_checked_before_scratch_creation() {
        let path = std::env::temp_dir().join(format!(
            "current-sqlite-cancel-{}-{}",
            std::process::id(),
            SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        assert!(
            read_snapshot_current_with_control(
                &path.join("absent.db"),
                None,
                &path,
                Instant::now() + Duration::from_secs(5),
                flag
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(path).unwrap();
    }
}
