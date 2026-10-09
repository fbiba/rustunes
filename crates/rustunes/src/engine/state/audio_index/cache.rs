//! Fingerprint cache for the complete audio index.
//!
//! A fingerprint is a pure function of a file's bytes. One stat names a phone
//! file: its device, path, size, and birth and modification times. Rewriting
//! the file changes its modification time and replacing it its birth time, so a
//! later capture that sees the same stat reuses the stored fingerprint instead
//! of copying the file over AFC. An entry is written after a full copy whose
//! SHA-256 equals the fingerprint, or after this host wrote those bytes there
//! (see `uploaded`). The phone's own hash (AFC GetFileHash) is never asked for:
//! each one leaks inside afcd, which stops opening files after about 2,550.
//! `Refresh` (full readback) copies everything and rewrites entries but never
//! reuses one. Without a configured scope nothing is cached.
//!
//! Entries live in their own SQLite database, `fingerprints.sqlite` in the
//! cache directory, never in a journal. Each row keeps one phone path's JSON
//! body with the file's stat and when it was stored and last used, under its
//! phone's key (see `phone_key`), not the digest journals name it by. Entries
//! outlive a change to how a build defines the ALAC fingerprint's composite, on
//! purpose: it is computed again from the stored fields, not checked, so such a
//! change copies no phone file again. A bad or unfamiliar entry is a miss. An
//! unreadable database, or one whose identity or table differs from this
//! build's, is set aside and rebuilt. Cache trouble never fails an index pass:
//! a failed lookup is a miss, and a failed write is reported once and skipped.
use super::receipt::{self, CodecDto, FingerprintDto};
use super::*;
use music_protocol::media::fingerprint;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};

mod covers;
pub(crate) use covers::{CoverProof, PhoneStat, cover_proof_known, cover_verified, verified_cover};

const SCHEMA: &str = "rustunes.fingerprint-cache-entry";
const MAX_ENTRY_BYTES: u64 = 64 * 1024;
const DATABASE: &str = "fingerprints.sqlite";
/// "ISFC" marks a rustunes fingerprint cache database.
const APPLICATION_ID: i64 = 0x4953_4643;
const TABLE: &str = "CREATE TABLE entries(device BLOB NOT NULL CHECK(length(device)=32), path TEXT NOT NULL CHECK(length(path) BETWEEN 1 AND 1024), file_bytes INTEGER NOT NULL CHECK(file_bytes>0), birthtime INTEGER NOT NULL, mtime INTEGER NOT NULL, body BLOB NOT NULL CHECK(length(body) BETWEEN 1 AND 65536), stored INTEGER NOT NULL, last_seen INTEGER NOT NULL, hits INTEGER NOT NULL DEFAULT 0 CHECK(hits>=0), writes INTEGER NOT NULL DEFAULT 1 CHECK(writes>=1), PRIMARY KEY(device,path)) STRICT, WITHOUT ROWID";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Reuse,
    Refresh,
}

#[derive(Clone)]
pub struct Config {
    directory: PathBuf,
    mode: Mode,
    shared: Arc<Shared>,
}
#[derive(Default)]
struct Shared {
    store: Mutex<Store>,
    /// Each phone this run selected: the digest callers name it by, and its
    /// phone key.
    phones: Mutex<Vec<([u8; 32], [u8; 32])>>,
    reused: AtomicU64,
    stored: AtomicU64,
    covers: Mutex<Store>,
    covers_reused: AtomicU64,
    covers_stored: AtomicU64,
    warned: AtomicBool,
}
#[derive(Default)]
enum Store {
    #[default]
    Unopened,
    Open(Connection),
    /// Opening failed for a reason other than damage; this run goes without.
    Unavailable,
}
impl Config {
    pub fn new(directory: PathBuf, mode: Mode) -> Self {
        Self {
            directory,
            mode,
            shared: Arc::default(),
        }
    }
    pub(super) fn reuses(&self) -> bool {
        self.mode == Mode::Reuse
    }
    #[cfg(test)]
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
    /// Run `f` on the database, opening it on first use. Without `create`, a
    /// cache directory that does not exist yet is a miss and creates nothing.
    fn with<T>(&self, create: bool, f: impl FnOnce(&Connection) -> Result<T>) -> Result<Option<T>> {
        self.with_in(&self.shared.store, open, create, f)
    }
    /// `with` for one of the cache's databases.
    fn with_in<T>(
        &self,
        store: &Mutex<Store>,
        open: fn(&Path) -> Result<Connection>,
        create: bool,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut store = store
            .lock()
            .map_err(|_| anyhow::anyhow!("fingerprint cache lock poisoned"))?;
        if matches!(*store, Store::Unopened) {
            if !create && !self.directory.exists() {
                return Ok(None);
            }
            match open(&self.directory) {
                Ok(connection) => *store = Store::Open(connection),
                Err(error) => {
                    *store = Store::Unavailable;
                    return Err(error);
                }
            }
        }
        match &*store {
            Store::Open(connection) => f(connection).map(Some),
            _ => Ok(None),
        }
    }
    fn warn(&self, error: &anyhow::Error) {
        if !self.shared.warned.swap(true, Ordering::Relaxed) {
            crate::engine::warn(
                crate::engine::Event::Observation(crate::engine::Observation::CacheUnavailable {
                    cache: "Fingerprint cache",
                    error: format!("{error:#}"),
                }),
                || format!("Fingerprint cache unavailable, continuing without it: {error:#}"),
            );
        }
    }
    fn select(&self, device: [u8; 32], key: [u8; 32]) {
        if let Ok(mut phones) = self.shared.phones.lock()
            && !phones.contains(&(device, key))
        {
            phones.push((device, key));
        }
    }
    /// The key of the phone callers name by `device`. A phone this run did not
    /// select has none, and its files are neither looked up nor stored.
    fn phone(&self, device: &[u8; 32]) -> Option<[u8; 32]> {
        let phones = self.shared.phones.lock().ok()?;
        phones
            .iter()
            .find(|(d, _)| d == device)
            .map(|&(_, key)| key)
    }
}

/// The key a phone's entries are stored under: the SHA-256 of its UDID, with no
/// domain or version tag. Journals name a phone by a tagged digest that a later
/// release may define differently; entries do not follow it, so they outlast
/// that release. Changing this key costs one copy of every file on every phone.
fn phone_key(udid: &str) -> [u8; 32] {
    Sha256::digest(udid.as_bytes()).into()
}
/// Note the phone this run selected, so the files it stats are looked up and
/// stored under its key. Outside a cache scope this does nothing.
pub(crate) fn phone_selected(udid: &str) {
    let Some(cache) = current() else { return };
    if let Ok(device) = crate::engine::state::journal::device_binding(udid) {
        cache.select(device, phone_key(udid));
    }
}

tokio::task_local! { static CURRENT: Config; }

/// Enable the cache for index passes inside this task, then print what it
/// reused and stored.
#[cfg(test)]
pub async fn scope<F: Future>(config: Option<Config>, future: F) -> F::Output {
    let (output, counts) = scope_counted(config, future).await;
    for (_, _, line) in counts.lines() {
        eprintln!("{line}");
    }
    output
}

/// What one cache scope reused and stored: (reused, stored) of fingerprints
/// and of cover proofs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub fingerprints: (u64, u64),
    pub covers: (u64, u64),
}

impl Counts {
    /// The summary line a run prints for each cache it used, after the
    /// cache's name and its (reused, stored) counts.
    pub fn lines(&self) -> Vec<(&'static str, (u64, u64), String)> {
        let mut lines = Vec::new();
        let (reused, stored) = self.fingerprints;
        if reused + stored > 0 {
            lines.push((
                "fingerprints",
                self.fingerprints,
                format!(
                    "Fingerprint cache: {reused} reused, {stored} stored after a verified copy"
                ),
            ));
        }
        let (reused, stored) = self.covers;
        if reused + stored > 0 {
            lines.push((
                "cover_proofs",
                self.covers,
                format!(
                    "Cover proof cache: {reused} reused, {stored} stored after a verified readback"
                ),
            ));
        }
        lines
    }
}

/// [`scope`], returning its counts instead of printing them.
pub async fn scope_counted<F: Future>(config: Option<Config>, future: F) -> (F::Output, Counts) {
    match config {
        Some(config) => {
            let shared = config.shared.clone();
            let output = CURRENT.scope(config, future).await;
            let counts = Counts {
                fingerprints: (
                    shared.reused.load(Ordering::Relaxed),
                    shared.stored.load(Ordering::Relaxed),
                ),
                covers: (
                    shared.covers_reused.load(Ordering::Relaxed),
                    shared.covers_stored.load(Ordering::Relaxed),
                ),
            };
            (output, counts)
        }
        None => (future.await, Counts::default()),
    }
}
pub(super) fn current() -> Option<Config> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Facts a verified copy or upload measured on the bytes a file key names.
pub(crate) struct VerifiedCopy {
    pub full_file_sha256: [u8; 32],
    /// MPEG layout and frame-span SHA-256; absent for every format but
    /// MPEG-1.
    pub mpeg: Option<(OtherCodec, [u8; 32])>,
}
/// The verified copy of this file, if any. Only inside a reusing scope.
pub(crate) fn verified_copy(key: &FileKey) -> Option<VerifiedCopy> {
    let cache = current().filter(Config::reuses)?;
    Some(match cache.get(key)? {
        Entry::Alac(fingerprint) => VerifiedCopy {
            full_file_sha256: fingerprint.full_file_sha256,
            mpeg: None,
        },
        Entry::Other {
            full_file_sha256,
            codec,
            frame_bytes_sha256,
        } => VerifiedCopy {
            full_file_sha256,
            mpeg: frame_bytes_sha256.map(|frames| (codec, frames)),
        },
    })
}
/// SHA-256 of an MPEG-1 file's frame span, measured on a local copy. Every
/// other format's identity already holds its payload digest.
pub(super) fn frame_bytes_sha256(path: &Path, codec: &OtherCodec) -> Result<Option<[u8; 32]>> {
    use std::io::{Read, Seek, SeekFrom};
    let OtherCodec::Mpeg1Layer3 {
        audio_offset,
        audio_bytes,
        ..
    } = codec
    else {
        return Ok(None);
    };
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*audio_offset))?;
    let mut digest = Sha256::new();
    let mut remaining = *audio_bytes;
    let mut buffer = vec![0; 64 * 1024];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buffer.len() as u64))?;
        let count = file.read(&mut buffer[..want])?;
        ensure!(count > 0, "MPEG frame span truncated");
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(Some(digest.finalize().into()))
}
/// Record the bytes this host just wrote to a phone file, so the capture that
/// follows reads its fingerprint instead of the file. The write, size, close
/// and rename replies and a stat of the stored size confirmed them, as Apple's
/// own sync checks an upload. `attrs` is the file's stat after its rename; the
/// phone then moves it to `path`, keeping the same stat.
pub(crate) fn uploaded(
    device: &[u8; 32],
    path: &str,
    attrs: &Attributes,
    source: &fingerprint::TransferredSource,
) {
    let Some(cache) = current() else { return };
    let Some(key) = FileKey::new(device, path, attrs) else {
        return;
    };
    if key.file_bytes != source.file_bytes {
        return;
    }
    let entry = match source.audio.mpeg() {
        None if !source.is_alac() => source
            .audio
            .encoded()
            .context("transferred audio has no identity")
            .map(|e| Entry::Other {
                full_file_sha256: source.full_file_sha256,
                codec: OtherCodec::Encoded(e.clone()),
                frame_bytes_sha256: None,
            }),
        None => source.require_alac().map(Entry::Alac),
        Some(m) => u32::try_from(m.trailing_bytes)
            .map_err(Into::into)
            .map(|trailing_tag_bytes| Entry::Other {
                full_file_sha256: source.full_file_sha256,
                codec: OtherCodec::Mpeg1Layer3 {
                    sample_rate: m.sample_rate,
                    channels: m.channels,
                    frame_count: m.frame_count,
                    audio_offset: m.audio_offset,
                    audio_bytes: m.audio_bytes,
                    trailing_tag_bytes,
                },
                frame_bytes_sha256: Some(m.frame_bytes_sha256),
            }),
    };
    match entry {
        Ok(entry) => cache.put(&key, &entry),
        Err(error) => cache.warn(&error),
    }
}
/// A phone file as one stat names it. A file without a birth or modification
/// time is never cached.
pub(crate) struct FileKey<'a> {
    device: &'a [u8; 32],
    path: &'a str,
    file_bytes: u64,
    birthtime: i64,
    mtime: i64,
}
impl<'a> FileKey<'a> {
    pub(crate) fn new(device: &'a [u8; 32], path: &'a str, attrs: &Attributes) -> Option<Self> {
        let number = |key: &str| -> Option<i64> { attrs.get(key)?.parse().ok() };
        if attrs.get("st_ifmt").map(String::as_str) != Some("S_IFREG") {
            return None;
        }
        Some(Self {
            device,
            path,
            file_bytes: u64::try_from(number("st_size")?).ok().filter(|&n| n > 0)?,
            birthtime: number("st_birthtime")?,
            mtime: number("st_mtime")?,
        })
    }
}

pub(super) enum Entry {
    Alac(SourceFingerprint),
    Other {
        full_file_sha256: [u8; 32],
        codec: OtherCodec,
        frame_bytes_sha256: Option<[u8; 32]>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryDto {
    schema: String,
    file_bytes: u64,
    #[serde(default)]
    fingerprint: Option<FingerprintDto>,
    #[serde(default)]
    other: Option<OtherDto>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OtherDto {
    full_file_sha256: String,
    codec: CodecDto,
    #[serde(default)]
    frame_bytes_sha256: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl Config {
    /// A missing, unreadable or invalid entry is a miss: the caller copies the
    /// file in full and writes a fresh entry.
    pub(super) fn get(&self, key: &FileKey) -> Option<Entry> {
        let phone = self.phone(key.device)?;
        let size = i64::try_from(key.file_bytes).ok()?;
        let body = self.with(false, |c| {
            Ok(c.query_row(
                "SELECT body FROM entries WHERE device=?1 AND path=?2 AND file_bytes=?3 AND birthtime=?4 AND mtime=?5",
                params![&phone[..], key.path, size, key.birthtime, key.mtime],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?)
        });
        let body = match body {
            Ok(body) => body.flatten()?,
            Err(error) => {
                self.warn(&error);
                return None;
            }
        };
        let entry = parse(&body, key.file_bytes).ok()?;
        // Use is bookkeeping for pruning; failing to record it keeps the hit.
        let _ = self.with(false, |c| {
            c.execute(
                "UPDATE entries SET last_seen=?3, hits=hits+1 WHERE device=?1 AND path=?2",
                params![&phone[..], key.path, unix_now()],
            )?;
            Ok(())
        });
        self.shared.reused.fetch_add(1, Ordering::Relaxed);
        Some(entry)
    }

    pub(super) fn put(&self, key: &FileKey, entry: &Entry) {
        let Some(phone) = self.phone(key.device) else {
            return;
        };
        let written = body(key.file_bytes, entry)
            .and_then(|body| self.with(true, |c| insert(c, &phone, key, &body, unix_now())));
        match written {
            Ok(Some(())) => {
                self.shared.stored.fetch_add(1, Ordering::Relaxed);
            }
            // The cache could not be opened earlier in this run; already reported.
            Ok(None) => {}
            Err(error) => self.warn(&error),
        }
    }
}

fn body(file_bytes: u64, entry: &Entry) -> Result<Vec<u8>> {
    let body = match entry {
        Entry::Alac(f) => {
            json!({"schema":SCHEMA,"file_bytes":file_bytes,"fingerprint":receipt::fingerprint_json(f)})
        }
        Entry::Other {
            full_file_sha256,
            codec,
            frame_bytes_sha256,
        } => {
            json!({"schema":SCHEMA,"file_bytes":file_bytes,
                "other":{"full_file_sha256":hex(full_file_sha256),"codec":receipt::codec_json(codec),
                "frame_bytes_sha256":frame_bytes_sha256.map(|h| hex(&h))}})
        }
    };
    let bytes = serde_json::to_vec(&body)?;
    parse(&bytes, file_bytes).context("fingerprint cache entry does not round-trip")?;
    Ok(bytes)
}

/// A path holds one file at a time: a new file's entry replaces the old one.
fn insert(c: &Connection, phone: &[u8; 32], key: &FileKey, body: &[u8], now: i64) -> Result<()> {
    c.execute(
        "INSERT INTO entries(device,path,file_bytes,birthtime,mtime,body,stored,last_seen) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
         ON CONFLICT(device,path) DO UPDATE SET file_bytes=excluded.file_bytes, birthtime=excluded.birthtime,
         mtime=excluded.mtime, body=excluded.body, stored=excluded.stored, last_seen=excluded.last_seen, writes=writes+1",
        params![
            &phone[..],
            key.path,
            i64::try_from(key.file_bytes)?,
            key.birthtime,
            key.mtime,
            body,
            now
        ],
    )?;
    Ok(())
}

fn parse(bytes: &[u8], file_bytes: u64) -> Result<Entry> {
    ensure!(
        bytes.len() as u64 <= MAX_ENTRY_BYTES,
        "fingerprint cache entry too large"
    );
    let dto: EntryDto = serde_json::from_slice(bytes)?;
    ensure!(
        dto.schema == SCHEMA && dto.file_bytes == file_bytes,
        "fingerprint cache entry names other bytes"
    );
    match (dto.fingerprint, dto.other) {
        (Some(fingerprint), None) => {
            let fingerprint = receipt::cached_fingerprint(fingerprint)?;
            ensure!(
                fingerprint.file_bytes == file_bytes,
                "fingerprint cache size differs"
            );
            Ok(Entry::Alac(fingerprint))
        }
        (None, Some(other)) => {
            ensure!(
                (1..=MAX_FILE_BYTES).contains(&file_bytes),
                "fingerprint cache size out of bounds"
            );
            let full_file_sha256 = receipt::hash(&other.full_file_sha256)?;
            let codec = receipt::codec(other.codec, file_bytes)?;
            let frame_bytes_sha256 = other
                .frame_bytes_sha256
                .as_deref()
                .map(receipt::hash)
                .transpose()?;
            // Every MPEG-1 entry records its frame span, and no other does.
            ensure!(
                matches!(codec, OtherCodec::Mpeg1Layer3 { .. }) == frame_bytes_sha256.is_some(),
                "fingerprint cache frame digest does not fit the codec"
            );
            Ok(Entry::Other {
                full_file_sha256,
                codec,
                frame_bytes_sha256,
            })
        }
        _ => bail!("fingerprint cache entry must hold exactly one fingerprint"),
    }
}

/// Open the cache database, creating it on first use. A damaged or unfamiliar
/// database is set aside and rebuilt; any other failure, such as a lock held
/// past the timeout, is returned.
fn open(directory: &Path) -> Result<Connection> {
    crate::engine::state::cache_database::open(
        directory,
        &crate::engine::state::cache_database::Schema {
            label: "fingerprint cache",
            file: DATABASE,
            application_id: APPLICATION_ID,
            tables: &[("entries", TABLE)],
        },
    )
}

/// What `cache stats` reports.
#[derive(Debug, serde::Serialize)]
pub struct Statistics {
    pub database: PathBuf,
    pub database_bytes: u64,
    pub entries: u64,
    pub entry_bytes: u64,
    /// Lookups that reused an entry instead of copying the file.
    pub hits: u64,
    /// Entries written after a verified copy (each one a copied file).
    pub writes: u64,
    pub oldest_last_seen_unix: Option<i64>,
    pub newest_last_seen_unix: Option<i64>,
    /// Entries no lookup or write has touched for `unseen_days`.
    pub unseen: u64,
    pub unseen_days: u64,
}
/// Report on the cache database without creating it.
pub fn statistics(directory: &Path, unseen_days: u64) -> Result<Statistics> {
    let database = directory.join(DATABASE);
    let mut report = Statistics {
        database: database.clone(),
        database_bytes: 0,
        entries: 0,
        entry_bytes: 0,
        hits: 0,
        writes: 0,
        oldest_last_seen_unix: None,
        newest_last_seen_unix: None,
        unseen: 0,
        unseen_days,
    };
    if !database.exists() {
        return Ok(report);
    }
    let c = open(directory)?;
    for suffix in ["", "-wal"] {
        let mut path = database.as_os_str().to_owned();
        path.push(suffix);
        report.database_bytes += fs::metadata(&path).map_or(0, |m| m.len());
    }
    let cutoff = cutoff(unseen_days)?;
    (
        report.entries,
        report.entry_bytes,
        report.hits,
        report.writes,
        report.oldest_last_seen_unix,
        report.newest_last_seen_unix,
        report.unseen,
    ) = c.query_row(
        "SELECT count(*), coalesce(sum(length(body)),0), coalesce(sum(hits),0), coalesce(sum(writes),0),
         min(last_seen), max(last_seen), coalesce(sum(last_seen<?1),0) FROM entries",
        [cutoff],
        |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
                r.get(4)?,
                r.get(5)?,
                r.get::<_, i64>(6)? as u64,
            ))
        },
    )?;
    Ok(report)
}
fn cutoff(unseen_days: u64) -> Result<i64> {
    let seconds = i64::try_from(
        unseen_days
            .checked_mul(86_400)
            .context("--unseen-days too large")?,
    )?;
    Ok(unix_now().saturating_sub(seconds))
}
/// Remove entries no lookup or write has touched for `unseen_days`, such as
/// tracks deleted from the phone, and cover proofs likewise. Returns how many
/// were (or would be) removed. A removed entry costs one copy if its file is
/// seen again.
pub fn prune(directory: &Path, unseen_days: u64, dry_run: bool) -> Result<u64> {
    let cutoff = cutoff(unseen_days)?;
    let covers = covers::prune(directory, cutoff, dry_run)?;
    if !directory.join(DATABASE).exists() {
        return Ok(covers);
    }
    let c = open(directory)?;
    let removed = if dry_run {
        c.query_row(
            "SELECT count(*) FROM entries WHERE last_seen<?1",
            [cutoff],
            |r| r.get::<_, i64>(0),
        )? as u64
    } else {
        let removed = c.execute("DELETE FROM entries WHERE last_seen<?1", [cutoff])? as u64;
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        removed
    };
    Ok(removed + covers)
}

/// Select a phone that tests name only by its digest.
#[cfg(test)]
pub(crate) fn select_for_tests(config: &Config, device: &[u8; 32]) {
    config.select(*device, Sha256::digest(device).into());
}
/// Seed an entry from a local file as a verified full copy would.
#[cfg(test)]
pub(crate) fn seed_for_tests(config: &Config, key: &FileKey, path: &Path) -> Result<()> {
    let entry = match fingerprint::read(path) {
        Ok(fingerprint) => Entry::Alac(fingerprint),
        Err(_) => {
            let (_, full_file_sha256, codec) = other::read(path)?;
            Entry::Other {
                frame_bytes_sha256: frame_bytes_sha256(path, &codec)?,
                full_file_sha256,
                codec,
            }
        }
    };
    config.put(key, &entry);
    ensure!(
        !config.shared.warned.load(Ordering::Relaxed),
        "seeding the fingerprint cache failed"
    );
    Ok(())
}
#[cfg(test)]
pub(super) fn database_for_tests(config: &Config) -> PathBuf {
    config.directory.join(DATABASE)
}
/// (reused, stored), as the end-of-run summary reports them.
#[cfg(test)]
pub(super) fn counts_for_tests(config: &Config) -> (u64, u64) {
    (
        config.shared.reused.load(Ordering::Relaxed),
        config.shared.stored.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_key_is_the_untagged_sha256_of_the_udid() {
        // Pinned: changing it makes every phone's files miss once.
        assert_eq!(
            hex(&phone_key("00008030-001A2B3C4D5E6F70")),
            "ee297680083bea81c354067b3d66292691e4319cb32f3c79e704ab90494148a2"
        );
    }

    #[tokio::test]
    async fn entries_outlast_a_redefined_device_digest() {
        let fixture =
            crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a");
        let attrs: Attributes = [
            ("st_ifmt", "S_IFREG".to_owned()),
            ("st_size", fs::metadata(&fixture).unwrap().len().to_string()),
            ("st_birthtime", "10".to_owned()),
            ("st_mtime", "20".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let dir = std::env::temp_dir().join(format!("fingerprint-phones-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = "iTunes_Control/Music/F00/ABCD.m4a";
        let (before, after) = ([1; 32], [2; 32]);
        let key = |device| FileKey::new(device, path, &attrs).unwrap();

        // Selecting a phone maps the digest journals name it by to its key.
        let selected = Config::new(dir.clone(), Mode::Reuse);
        scope(Some(selected.clone()), async { phone_selected("phone") }).await;
        let binding = crate::engine::state::journal::device_binding("phone").unwrap();
        assert_eq!(selected.phone(&binding), Some(phone_key("phone")));

        // A phone this run did not select is neither stored nor looked up.
        let unselected = Config::new(dir.clone(), Mode::Reuse);
        seed_for_tests(&unselected, &key(&before), &fixture).unwrap();
        assert_eq!(counts_for_tests(&unselected), (0, 0));

        let first = Config::new(dir.clone(), Mode::Reuse);
        first.select(before, phone_key("phone"));
        seed_for_tests(&first, &key(&before), &fixture).unwrap();
        assert_eq!(counts_for_tests(&first), (0, 1));

        // A release that names the same phone by another digest reuses it.
        let upgraded = Config::new(dir.clone(), Mode::Reuse);
        upgraded.select(after, phone_key("phone"));
        assert!(upgraded.get(&key(&after)).is_some());
        assert!(upgraded.get(&key(&before)).is_none());
        let other = Config::new(dir.clone(), Mode::Reuse);
        other.select(after, phone_key("another phone"));
        assert!(other.get(&key(&after)).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }
}
