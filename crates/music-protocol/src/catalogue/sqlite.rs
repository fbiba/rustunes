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

#[derive(Clone)]
pub struct SourceHashes {
    pub database: [u8; 32],
    pub wal: Option<[u8; 32]>,
    pub database_bytes: u64,
    pub wal_bytes: Option<u64>,
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

type TableSpec = (
    &'static str,
    Option<&'static str>,
    &'static [(&'static str, &'static str)],
);
