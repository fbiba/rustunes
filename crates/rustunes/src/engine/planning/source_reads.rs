//! Planning reads of local sources, kept by file identity for this process and,
//! once [`persist`] names a directory, across runs. Planning, a saved plan's
//! revalidation and a dry run's live preflight read the same files seconds
//! apart, and the next run reads them again; each read hashes the whole file.
//! A file whose path, device, inode, size, mtime or ctime changed is read
//! again, and a read during which any of them changed is not kept. Uploads,
//! pre-send checks and recovery read the bytes themselves.
//!
//! Across runs, reads live in `sources.sqlite`, never in a journal. A cover is
//! kept once by its SHA-256 and checked against it when used. A row older than
//! [`KEEP_DAYS`] is dropped, so an unchanged file is read once more after that.
//! Rows outlive a change to how a build defines the ALAC fingerprint's
//! composite, on purpose: it is computed again from the stored fields, so such a
//! change reads no source file again.
//! Cache trouble never fails planning: a failed lookup is a miss, and a failed
//! write is reported once and skipped.
use crate::engine::state::cache_database;
use anyhow::{Result, ensure};
use music_protocol::media::artwork;
use music_protocol::media::fingerprint;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

const WORKERS: usize = 8;
const KEEP_DAYS: i64 = 180;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
fn key(path: &Path) -> Option<Key> {
    let m = std::fs::symlink_metadata(path).ok()?;
    m.is_file().then(|| Key {
        path: path.to_owned(),
        dev: m.dev(),
        ino: m.ino(),
        size: m.size(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
    })
}
impl Key {
    fn stat(&self) -> Vec<u8> {
        [
            self.dev.to_be_bytes(),
            self.ino.to_be_bytes(),
            self.size.to_be_bytes(),
            self.mtime.0.to_be_bytes(),
            self.mtime.1.to_be_bytes(),
            self.ctime.0.to_be_bytes(),
            self.ctime.1.to_be_bytes(),
        ]
        .concat()
    }
    fn path(&self) -> &[u8] {
        self.path.as_os_str().as_bytes()
    }
}

static FINGERPRINTS: Mutex<Option<HashMap<Key, fingerprint::SourceFingerprint>>> = Mutex::new(None);
static ARTWORK: Mutex<Option<HashMap<Key, artwork::SourceArtwork>>> = Mutex::new(None);
static DISK: OnceLock<Disk> = OnceLock::new();

fn kept<T: Clone + Persisted>(
    store: &Mutex<Option<HashMap<Key, T>>>,
    path: &Path,
    read: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let before = key(path);
    if let Some(before) = &before {
        if let Some(value) = store
            .lock()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| s.get(before).cloned()))
        {
            return Ok(value);
        }
        if let Some(value) = DISK.get().and_then(|disk| disk.load::<T>(before)) {
            if let Ok(mut store) = store.lock() {
                store
                    .get_or_insert_default()
                    .insert(before.clone(), value.clone());
            }
            return Ok(value);
        }
    }
    let value = read(path)?;
    if let Some(before) = before
        && key(path).as_ref() == Some(&before)
    {
        if let Some(disk) = DISK.get() {
            disk.store(&before, &value);
        }
        if let Ok(mut store) = store.lock() {
            store.get_or_insert_default().insert(before, value.clone());
        }
    }
    Ok(value)
}

/// `fingerprint::read`, unless this file was read unchanged before.
pub(crate) fn fingerprint(path: &Path) -> Result<fingerprint::SourceFingerprint> {
    kept(&FINGERPRINTS, path, fingerprint::read)
}

/// `artwork::read`, unless this file was read unchanged before.
pub(crate) fn artwork(path: &Path) -> Result<artwork::SourceArtwork> {
    kept(&ARTWORK, path, artwork::read)
}

/// Keep reads across runs in `directory`. Without `reuse` (full readback),
/// every file is read and its rows rewritten. Only the first call counts.
pub(crate) fn persist(directory: PathBuf, reuse: bool) {
    let _ = DISK.set(Disk::new(directory, reuse));
}

/// How many reads the cross-run cache answered and stored, if it was used.
pub(crate) fn report() -> Option<(u64, u64)> {
    let disk = DISK.get()?;
    let counts = (
        disk.reused.load(Ordering::Relaxed),
        disk.stored.load(Ordering::Relaxed),
    );
    (counts != (0, 0)).then_some(counts)
}

/// Reads every M4A's fingerprint and artwork on a few threads, so the
/// sequential passes that follow find them kept. Failures are left for those
/// passes to report.
pub(crate) fn warm<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    let paths: Vec<&Path> = paths
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("m4a")))
        .collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(WORKERS)
        .min(paths.len());
    if workers < 2 {
        return;
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(path) = paths.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let _ = fingerprint(path);
                    let _ = artwork(path);
                }
            });
        }
    });
}

const SCHEMA: cache_database::Schema = cache_database::Schema {
    label: "source cache",
    file: "sources.sqlite",
    // "ISSC" marks a rustunes source cache database.
    application_id: 0x4953_5343,
    tables: &[
        (
            "artwork",
            "CREATE TABLE artwork(path BLOB PRIMARY KEY CHECK(length(path) BETWEEN 1 AND 4096), stat BLOB NOT NULL CHECK(length(stat)=56), source_sha256 BLOB NOT NULL CHECK(length(source_sha256)=32), cover_sha256 BLOB CHECK(length(cover_sha256)=32), cover_checksum INTEGER CHECK((cover_sha256 IS NULL)=(cover_checksum IS NULL)), embedded_format TEXT CHECK(embedded_format IN ('png','gif','bmp')), embedded_sha256 BLOB CHECK((embedded_format IS NULL)=(embedded_sha256 IS NULL) AND length(embedded_sha256)=32), embedded_width INTEGER CHECK((embedded_format IS NULL)=(embedded_width IS NULL)), embedded_height INTEGER CHECK((embedded_format IS NULL)=(embedded_height IS NULL)), left_out TEXT CHECK(left_out IS NULL OR cover_sha256 IS NULL), stored INTEGER NOT NULL) STRICT, WITHOUT ROWID",
        ),
        (
            "covers",
            "CREATE TABLE covers(sha256 BLOB PRIMARY KEY CHECK(length(sha256)=32), bytes BLOB NOT NULL CHECK(length(bytes)>0)) STRICT, WITHOUT ROWID",
        ),
        (
            "fingerprints",
            "CREATE TABLE fingerprints(path BLOB PRIMARY KEY CHECK(length(path) BETWEEN 1 AND 4096), stat BLOB NOT NULL CHECK(length(stat)=56), full_file_sha256 BLOB NOT NULL CHECK(length(full_file_sha256)=32), audio_sha256 BLOB NOT NULL CHECK(length(audio_sha256)=32), alac_config_sha256 BLOB NOT NULL CHECK(length(alac_config_sha256)=32), sample_sizes_sha256 BLOB NOT NULL CHECK(length(sample_sizes_sha256)=32), media_sha256 BLOB NOT NULL CHECK(length(media_sha256)=32), timescale INTEGER NOT NULL, duration_ticks INTEGER NOT NULL, media_bytes INTEGER NOT NULL, stored INTEGER NOT NULL) STRICT, WITHOUT ROWID",
        ),
    ],
};

struct Disk {
    directory: PathBuf,
    reuse: bool,
    connection: Mutex<Option<Result<Connection, ()>>>,
    reused: AtomicU64,
    stored: AtomicU64,
    warned: AtomicBool,
}
impl Disk {
    fn new(directory: PathBuf, reuse: bool) -> Self {
        Self {
            directory,
            reuse,
            connection: Mutex::new(None),
            reused: AtomicU64::new(0),
            stored: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
    }
    /// Run `f` on the database, opening it on first use. Without `create`, a
    /// cache directory that does not exist yet is a miss and creates nothing.
    fn with<T>(&self, create: bool, f: impl FnOnce(&Connection) -> Result<T>) -> Option<T> {
        let mut connection = self.connection.lock().ok()?;
        if connection.is_none() {
            if !create && !self.directory.exists() {
                return None;
            }
            *connection = Some(open(&self.directory).map_err(|error| self.warn(&error)));
        }
        let Some(Ok(c)) = connection.as_ref() else {
            return None;
        };
        f(c).map_err(|error| self.warn(&error)).ok()
    }
    fn load<T: Persisted>(&self, key: &Key) -> Option<T> {
        if !self.reuse {
            return None;
        }
        let value = self.with(false, |c| T::load(c, key))??;
        self.reused.fetch_add(1, Ordering::Relaxed);
        Some(value)
    }
    fn store<T: Persisted>(&self, key: &Key, value: &T) {
        if self.with(true, |c| value.store(c, key)).is_some() {
            self.stored.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn warn(&self, error: &anyhow::Error) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            crate::engine::warn(
                crate::engine::Event::Observation(crate::engine::Observation::CacheUnavailable {
                    cache: "Source cache",
                    error: format!("{error:#}"),
                }),
                || format!("Source cache unavailable, continuing without it: {error:#}"),
            );
        }
    }
}

/// Open the database and drop rows older than [`KEEP_DAYS`] and covers no row
/// names.
fn open(directory: &Path) -> Result<Connection> {
    let c = cache_database::open(directory, &SCHEMA)?;
    let cutoff = unix_now() - KEEP_DAYS * 86_400;
    c.execute("DELETE FROM fingerprints WHERE stored<?1", [cutoff])?;
    c.execute("DELETE FROM artwork WHERE stored<?1", [cutoff])?;
    c.execute(
        "DELETE FROM covers WHERE sha256 NOT IN (SELECT cover_sha256 FROM artwork WHERE cover_sha256 IS NOT NULL)",
        [],
    )?;
    Ok(c)
}
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
fn digest(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("source cache digest is not 32 bytes"))
}

/// A read the cross-run cache can hold. `load` answers `None` for a file it
/// has no row for at exactly this identity.
trait Persisted: Sized {
    fn load(c: &Connection, key: &Key) -> Result<Option<Self>>;
    fn store(&self, c: &Connection, key: &Key) -> Result<()>;
}

impl Persisted for fingerprint::SourceFingerprint {
    fn load(c: &Connection, key: &Key) -> Result<Option<Self>> {
        let Some(row) = c
            .query_row(
                "SELECT full_file_sha256, audio_sha256, alac_config_sha256, sample_sizes_sha256, media_sha256, timescale, duration_ticks, media_bytes FROM fingerprints WHERE path=?1 AND stat=?2",
                params![key.path(), key.stat()],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let (full, _stored_composite, config, sizes, media, timescale, ticks, media_bytes) = row;
        let mut audio = fingerprint::EncodedAlac {
            sha256: [0; 32],
            alac_config_sha256: digest(config)?,
            sample_sizes_sha256: digest(sizes)?,
            media_sha256: digest(media)?,
            timescale: u32::try_from(timescale)?,
            duration_ticks: u64::try_from(ticks)?,
            media_bytes: u64::try_from(media_bytes)?,
        };
        // The composite is derived from the other fields. Computing it again
        // keeps a row usable, on purpose, when this build defines it
        // differently from the build that stored it, instead of handing
        // planning a stale identity.
        audio.sha256 = audio.composite_sha256();
        Ok(Some(Self {
            file_bytes: key.size,
            full_file_sha256: digest(full)?,
            audio,
        }))
    }
    fn store(&self, c: &Connection, key: &Key) -> Result<()> {
        ensure!(
            self.file_bytes == key.size,
            "fingerprint size differs from its file"
        );
        let a = &self.audio;
        c.execute(
            "INSERT OR REPLACE INTO fingerprints VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                key.path(),
                key.stat(),
                &self.full_file_sha256[..],
                &a.sha256[..],
                &a.alac_config_sha256[..],
                &a.sample_sizes_sha256[..],
                &a.media_sha256[..],
                a.timescale,
                i64::try_from(a.duration_ticks)?,
                i64::try_from(a.media_bytes)?,
                unix_now(),
            ],
        )?;
        Ok(())
    }
}

impl Persisted for artwork::SourceArtwork {
    fn load(c: &Connection, key: &Key) -> Result<Option<Self>> {
        let Some(row) = c
            .query_row(
                "SELECT source_sha256, cover_sha256, cover_checksum, embedded_format, embedded_sha256, embedded_width, embedded_height, left_out FROM artwork WHERE path=?1 AND stat=?2",
                params![key.path(), key.stat()],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Option<Vec<u8>>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        (
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<Vec<u8>>>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, Option<i64>>(6)?,
                        ),
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let (source, cover_sha256, checksum, embedded, left_out) = row;
        let cover = match (cover_sha256, checksum) {
            (Some(sha256), Some(checksum)) => {
                let sha256 = digest(sha256)?;
                let Some(bytes) = c
                    .query_row(
                        "SELECT bytes FROM covers WHERE sha256=?1",
                        [&sha256[..]],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .optional()?
                else {
                    return Ok(None);
                };
                ensure!(
                    <[u8; 32]>::from(Sha256::digest(&bytes)) == sha256,
                    "source cache cover does not match its SHA-256"
                );
                let cover = artwork::Cover {
                    sha256,
                    checksum: u32::try_from(checksum)?,
                    bytes,
                };
                ensure!(
                    u64::from(cover.checksum)
                        == cover.bytes.len() as u64 + u64::from(artwork::MP4_CHECKSUM_HEADER),
                    "source cache cover checksum does not match its length"
                );
                Some(cover)
            }
            _ => None,
        };
        let embedded = match embedded {
            (Some(format), Some(sha256), Some(width), Some(height)) => {
                Some(artwork::EmbeddedImage {
                    format: match format.as_str() {
                        "png" => "png",
                        "gif" => "gif",
                        "bmp" => "bmp",
                        other => anyhow::bail!("source cache image format {other:?} is unknown"),
                    },
                    sha256: digest(sha256)?,
                    width: u16::try_from(width)?,
                    height: u16::try_from(height)?,
                })
            }
            _ => None,
        };
        Ok(Some(Self {
            source_size: key.size,
            source_sha256: digest(source)?,
            cover,
            embedded,
            left_out,
        }))
    }
    fn store(&self, c: &Connection, key: &Key) -> Result<()> {
        ensure!(
            self.source_size == key.size,
            "artwork size differs from its file"
        );
        if let Some(cover) = &self.cover {
            c.execute(
                "INSERT OR IGNORE INTO covers VALUES (?1,?2)",
                params![&cover.sha256[..], cover.bytes],
            )?;
        }
        let image = self.embedded.as_ref();
        c.execute(
            "INSERT OR REPLACE INTO artwork VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                key.path(),
                key.stat(),
                &self.source_sha256[..],
                self.cover.as_ref().map(|cover| &cover.sha256[..]),
                self.cover.as_ref().map(|cover| cover.checksum),
                image.map(|image| image.format),
                image.map(|image| &image.sha256[..]),
                image.map(|image| image.width),
                image.map(|image| image.height),
                self.left_out,
                unix_now(),
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
impl Persisted for Vec<u8> {
    fn load(_: &Connection, _: &Key) -> Result<Option<Self>> {
        Ok(None)
    }
    fn store(&self, _: &Connection, _: &Key) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
