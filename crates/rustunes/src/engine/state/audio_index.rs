//! Read-only, content-based index of every represented local Music asset.
//!
//! A successful index binds exact catalog DB/WAL bytes around the entire pass.
//! This is not an atomic device snapshot. Per-file events are useful incomplete
//! evidence if later catalog drift, cancellation, or an unsupported file aborts
//! the pass; they must not be treated as proof that another song is absent.
//! The caller supplies verified device/library identities and the sole connected
//! AFC client. No discovery, mutation, retry or packet trace occurs. Inside a
//! `cache::scope`, a file whose device, path and stat match a cached entry
//! reuses that fingerprint instead of being copied (see `cache`).
//! Bounds apply to accepted chunks; the pinned AFC transport may allocate a
//! malformed packet before this module can reject its decoded payload.

use anyhow::{Context, Result, bail, ensure};
use idevice::afc::AfcClient;
use music_protocol::protocol::afc::stat::{
    self as afc_stat, regular_size, unchanged, without_access_time,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, DirBuilder, File, OpenOptions},
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, sleep, sleep_until};

use music_protocol::catalogue::sqlite::{Catalog, SourceHashes, Track};
use music_protocol::media::fingerprint::{self, SourceFingerprint};

pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_TRACKS: usize = 50_000;
const CATALOG_NAMES: [&str; 3] = [
    "MediaLibrary.sqlitedb",
    "MediaLibrary.sqlitedb-wal",
    "MediaLibrary.sqlitedb-shm",
];
static SPOOL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub type Attributes = BTreeMap<String, String>;

#[derive(Clone, PartialEq, Eq)]
pub struct IndexIdentity {
    pub device_identifier_sha256: [u8; 32],
    /// LibraryIdentity::binding_sha256(), binding both persisted components.
    pub library_identity_sha256: [u8; 32],
}

#[derive(Clone, PartialEq, Eq)]
pub struct CatalogBinding {
    pub database_sha256: [u8; 32],
    pub database_bytes: u64,
    pub wal_sha256: Option<[u8; 32]>,
    pub wal_bytes: Option<u64>,
    pub persisted_media_anchor: u32,
}
impl CatalogBinding {
    fn from_catalog(catalog: &Catalog) -> Self {
        let SourceHashes {
            database,
            database_bytes,
            wal,
            wal_bytes,
        } = catalog.source_hashes();
        Self {
            database_sha256: *database,
            database_bytes: *database_bytes,
            wal_sha256: *wal,
            wal_bytes: *wal_bytes,
            persisted_media_anchor: catalog.persisted_media_anchor(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TrackAudio {
    pub sync_id: u64,
    pub afc_path: String,
    /// Stable attributes with access time excluded. This is not a content hash.
    pub attributes: Attributes,
    pub fingerprint: SourceFingerprint,
}

#[derive(Clone, PartialEq, Eq)]
pub struct OtherAudio {
    pub sync_id: u64,
    pub afc_path: String,
    pub attributes: Attributes,
    pub file_bytes: u64,
    pub full_file_sha256: [u8; 32],
    pub codec: OtherCodec,
}

/// Every attribute but `st_blocks` is equal. Some minutes after writing a
/// file the phone trims its spare allocation; its content, size and times
/// stay as they were.
fn same_attributes(a: &Attributes, b: &Attributes) -> bool {
    a.iter()
        .filter(|(k, _)| k.as_str() != "st_blocks")
        .eq(b.iter().filter(|(k, _)| k.as_str() != "st_blocks"))
}

impl TrackAudio {
    /// The same unchanged file as `other`; only its allocation may differ.
    pub fn same_file(&self, other: &Self) -> bool {
        self.sync_id == other.sync_id
            && self.afc_path == other.afc_path
            && self.fingerprint == other.fingerprint
            && same_attributes(&self.attributes, &other.attributes)
    }
}

impl OtherAudio {
    /// The same unchanged file as `other`; only its allocation may differ.
    pub fn same_file(&self, other: &Self) -> bool {
        self.sync_id == other.sync_id
            && self.afc_path == other.afc_path
            && self.file_bytes == other.file_bytes
            && self.full_file_sha256 == other.full_file_sha256
            && self.codec == other.codec
            && same_attributes(&self.attributes, &other.attributes)
    }
}

/// Rows a capture may find without audio. `unfilled` names rows an
/// interrupted insertion left with no file location; `any_unfilled` accepts
/// every such row, for a caller that accounts for them once it knows the
/// journal; `gone` lets any catalogued file be missing, as when the phone
/// deleted a target's file and then rolled its removal session back. The
/// index lists each kind apart and callers decide which rows they accept.
#[derive(Clone, Copy)]
pub struct Absent<'a> {
    pub unfilled: &'a BTreeSet<u64>,
    pub any_unfilled: bool,
    pub gone: bool,
}

static NO_ROWS: BTreeSet<u64> = BTreeSet::new();

impl<'a> Absent<'a> {
    /// Every row holds its audio.
    pub const NONE: Absent<'static> = Absent {
        unfilled: &NO_ROWS,
        any_unfilled: false,
        gone: false,
    };
    pub fn unfilled(rows: &'a BTreeSet<u64>) -> Self {
        Self {
            unfilled: rows,
            any_unfilled: false,
            gone: false,
        }
    }
    /// The rows without audio this accepts in `catalog`.
    pub(crate) fn rows(&self, catalog: &Catalog) -> BTreeSet<u64> {
        match self.any_unfilled {
            true => catalog
                .tracks()
                .iter()
                .filter(|track| unfilled_row(track))
                .map(|track| track.sync_id)
                .collect(),
            false => self.unfilled.clone(),
        }
    }
}

pub struct DeviceAudioIndex {
    identity: IndexIdentity,
    catalog: CatalogBinding,
    tracks: BTreeMap<u64, TrackAudio>,
    other_audio: BTreeMap<u64, OtherAudio>,
    /// Rows an interrupted insertion left without audio, which the capture was
    /// told to expect (`read_all_allowing`). Empty for every other index.
    unfilled: BTreeSet<u64>,
    /// Catalogued rows whose file is missing, when the capture allowed it
    /// (`Absent::gone`). Empty for every other index.
    gone: BTreeSet<u64>,
}
impl DeviceAudioIndex {
    pub fn identity(&self) -> &IndexIdentity {
        &self.identity
    }
    pub fn catalog(&self) -> &CatalogBinding {
        &self.catalog
    }
    pub fn tracks(&self) -> &BTreeMap<u64, TrackAudio> {
        &self.tracks
    }
    pub fn other_audio(&self) -> &BTreeMap<u64, OtherAudio> {
        &self.other_audio
    }
    pub fn unfilled(&self) -> &BTreeSet<u64> {
        &self.unfilled
    }
    pub fn gone(&self) -> &BTreeSet<u64> {
        &self.gone
    }
}

/// The caller owns cancellation and the overall deadline (for example 30 min).
/// Each AFC request also has its own bound (`afc::copy::OPERATION`, thirty
/// seconds). Local fingerprinting completes its current bounded regular-file
/// read before noticing a new cancellation.
pub struct ReadControl<'a> {
    pub deadline: Instant,
    pub cancelled: &'a AtomicBool,
}

/// A valid catalogue guard mismatch before any index progress or audio read.
/// Other failures and guard mismatches after enumeration never have this type.
#[derive(Debug)]
pub(crate) struct InitialCatalogGuardDrift(anyhow::Error);

impl std::fmt::Display for InitialCatalogGuardDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for InitialCatalogGuardDrift {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// A valid catalogue guard mismatch after a complete pass: the phone wrote its
/// catalogue meanwhile (it does so between sessions), so the capture may read
/// once more.
#[derive(Debug)]
pub(crate) struct LateCatalogGuardDrift(anyhow::Error);

impl std::fmt::Display for LateCatalogGuardDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
impl std::error::Error for LateCatalogGuardDrift {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Debug)]
struct CatalogGuardDrift(&'static str);
impl std::fmt::Display for CatalogGuardDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CatalogGuardDrift {}

/// No raw media bytes are emitted. Fingerprinted is delivered before continuing
/// to another file; the callback may durably retain it or fail to stop the pass.
pub enum IndexProgress {
    Started {
        track_count: usize,
        total_bytes: u64,
        identity: IndexIdentity,
        catalog: CatalogBinding,
    },
    FileStarted {
        sync_id: u64,
        afc_path: String,
        file_bytes: u64,
    },
    FileBytes {
        sync_id: u64,
        file_bytes_read: u64,
    },
    /// A cached fingerprint replaced the file's copy; its row follows.
    Reused {
        sync_id: u64,
    },
    Fingerprinted(TrackAudio),
    OtherFingerprinted(OtherAudio),
}

#[derive(Clone)]
struct Target {
    sync_id: u64,
    path: String,
    bytes: u64,
}

/// Validate scope before opening a connector: the bytes of every track's
/// audio, skipping rows without audio among `allowed`. No file or device I/O.
pub fn required_bytes_allowing(catalog: &Catalog, allowed: &BTreeSet<u64>) -> Result<u64> {
    Ok(targets_allowing(catalog, allowed)?
        .0
        .iter()
        .map(|target| target.bytes)
        .sum())
}

/// The row an insertion leaves when its audio never arrives: still in the
/// library and supported, with no file location, size or local asset.
fn unfilled_row(track: &Track) -> bool {
    let available = &track.availability;
    track.afc_path.is_none()
        && track.metadata.file_size == 0
        && !available.is_placeholder
        && available.is_library_added
        && !available.has_non_purgeable_asset
        && available.is_supported_type
}

fn target(track: &Track) -> Result<Target> {
    let available = &track.availability;
    ensure!(
        !available.is_placeholder
            && available.is_library_added
            && available.has_non_purgeable_asset
            && available.is_supported_type,
        "catalog contains an unavailable or unsupported track"
    );
    ensure!(
        track.sync_id != 0 && (1..=MAX_FILE_BYTES).contains(&track.metadata.file_size),
        "track identity/size exceeds index scope"
    );
    let path = track
        .afc_path
        .as_ref()
        .context("catalog track lacks local Music location")?;
    ensure!(
        safe_path(path),
        "catalog track location is outside safe Music scope"
    );
    Ok(Target {
        sync_id: track.sync_id,
        path: path.clone(),
        bytes: track.metadata.file_size,
    })
}

/// Every track's audio, except rows without audio among `allowed`, which are
/// returned apart. Any other row without audio is refused.
fn targets_allowing(
    catalog: &Catalog,
    allowed: &BTreeSet<u64>,
) -> Result<(Vec<Target>, BTreeSet<u64>)> {
    ensure!(
        catalog.tracks().len() <= MAX_TRACKS,
        "audio index track bound exceeded"
    );
    let mut result = Vec::new();
    let mut unfilled = BTreeSet::new();
    let mut paths = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut total = 0u64;
    for track in catalog.tracks() {
        if allowed.contains(&track.sync_id) && unfilled_row(track) {
            ensure!(
                ids.insert(track.sync_id, ()).is_none(),
                "ambiguous audio index identity or path"
            );
            unfilled.insert(track.sync_id);
            continue;
        }
        let target = target(track)?;
        ensure!(
            paths.insert(target.path.to_ascii_lowercase(), ()).is_none()
                && ids.insert(target.sync_id, ()).is_none(),
            "ambiguous audio index identity or path"
        );
        total = total
            .checked_add(target.bytes)
            .context("audio index byte overflow")?;
        ensure!(total <= MAX_TOTAL_BYTES, "audio index exceeds 64 GiB bound");
        result.push(target);
    }
    result.sort_by_key(|target| target.sync_id);
    Ok((result, unfilled))
}

fn safe_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').take(5).collect();
    parts.len() == 4
        && parts[0] == "iTunes_Control"
        && parts[1] == "Music"
        && parts[2].len() == 3
        && parts[2].starts_with('F')
        && parts[2].as_bytes()[1..].iter().all(u8::is_ascii_digit)
        && !parts[3].is_empty()
        && parts[3].len() <= 255
        && !matches!(parts[3], "." | "..")
        && parts[3]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The refusal for a catalogued file that is not on the phone.
fn missing_file(target: &Target) -> String {
    format!(
        "catalog audio file {} of track {:016x} is missing; index incomplete (a removal cut short can leave this: running it again finishes it)",
        target.path, target.sync_id
    )
}

fn check_control(control: &ReadControl<'_>) -> Result<()> {
    ensure!(
        !control.cancelled.load(Ordering::Relaxed),
        "audio index cancelled; partial evidence only"
    );
    ensure!(
        Instant::now() < control.deadline,
        "audio index deadline expired; partial evidence only"
    );
    Ok(())
}
async fn controlled<T>(control: &ReadControl<'_>, future: impl Future<Output = T>) -> Result<T> {
    check_control(control)?;
    let cancelled = async {
        loop {
            if control.cancelled.load(Ordering::Relaxed) {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::select! {
        result = future => Ok(result),
        _ = cancelled => bail!("audio index cancelled; connection discarded"),
        _ = sleep_until(control.deadline) => bail!("audio index deadline expired; connection discarded"),
    }
}

async fn stat(
    afc: &mut AfcClient,
    path: &str,
    control: &ReadControl<'_>,
) -> Result<Option<Attributes>> {
    controlled(
        control,
        afc_stat::attributes(
            afc,
            path,
            control.deadline,
            None,
            "stat read-only index input",
        ),
    )
    .await?
}

/// Each path's stat as `stat` gives it, with a window of requests in flight
/// (`afc::stat::pipelined`). Any failure discards the connection, as one
/// failed stat does.
async fn stat_all(
    afc: &mut AfcClient,
    paths: &[&str],
    control: &ReadControl<'_>,
) -> Result<Vec<Option<Attributes>>> {
    use music_protocol::protocol::afc::stat::{STAT_WINDOW, pipelined};
    let mut stats = Vec::with_capacity(paths.len());
    for window in paths.chunks(STAT_WINDOW) {
        let replies = controlled(
            control,
            music_protocol::protocol::afc::copy::bounded(
                control.deadline,
                "stat read-only index inputs",
                pipelined(afc, window),
            ),
        )
        .await?
        .map_err(|failure| failure.error)?;
        for reply in replies {
            stats.push(afc_stat::reply_attributes(
                reply,
                "stat read-only index inputs",
            )?);
        }
    }
    Ok(stats)
}

async fn copy(
    afc: &mut AfcClient,
    path: &str,
    size: u64,
    maximum: u64,
    output: &mut impl Write,
    control: &ReadControl<'_>,
) -> Result<()> {
    ensure!(size <= maximum, "index input exceeds copy bound");
    let handle = controlled(
        control,
        music_protocol::protocol::afc::copy::open_readonly(
            afc,
            path,
            control.deadline,
            None,
            "open index input read-only",
        ),
    )
    .await??;
    let (copied, closed) = controlled(
        control,
        music_protocol::protocol::afc::copy::transfer(
            afc,
            handle,
            output,
            size,
            music_protocol::protocol::afc::copy::CopyLimits {
                maximum_size: maximum,
                maximum_reads: usize::try_from(
                    maximum.div_ceil(music_protocol::protocol::afc::copy::CHUNK as u64),
                )? * 4,
            },
            control.deadline,
        ),
    )
    .await?;
    copied.map_err(|failure| failure.error)?;
    closed
        .context("index handle close unattempted; connection discarded")?
        .map_err(|failure| failure.error)?;
    Ok(())
}

async fn catalog_stats(
    afc: &mut AfcClient,
    control: &ReadControl<'_>,
) -> Result<Vec<Option<Attributes>>> {
    let mut rows = Vec::new();
    for name in CATALOG_NAMES {
        rows.push(
            stat(afc, &format!("iTunes_Control/iTunes/{name}"), control)
                .await?
                .map(without_access_time),
        );
    }
    Ok(rows)
}
async fn verify_catalog(
    afc: &mut AfcClient,
    binding: &CatalogBinding,
    control: &ReadControl<'_>,
) -> Result<Vec<Option<Attributes>>> {
    ensure!(
        (1..=music_protocol::catalogue::sqlite::MAX_DATABASE_BYTES)
            .contains(&binding.database_bytes)
            && binding.wal_bytes.is_some() == binding.wal_sha256.is_some()
            && binding
                .wal_bytes
                .is_none_or(|size| size <= music_protocol::catalogue::sqlite::MAX_WAL_BYTES),
        "invalid catalogue binding shape"
    );
    let before = catalog_stats(afc, control).await?;
    let (database_bytes, wal_bytes) = catalog_sizes(&before)?;
    check_control(control)?;
    if database_bytes != binding.database_bytes {
        return Err(CatalogGuardDrift("live catalog database size differs").into());
    }
    if wal_bytes != binding.wal_bytes {
        return Err(CatalogGuardDrift("live catalog WAL presence/size differs").into());
    }
    for (index, expected, size, limit) in [
        (
            0,
            Some(binding.database_sha256),
            Some(binding.database_bytes),
            music_protocol::catalogue::sqlite::MAX_DATABASE_BYTES,
        ),
        (
            1,
            binding.wal_sha256,
            binding.wal_bytes,
            music_protocol::catalogue::sqlite::MAX_WAL_BYTES,
        ),
    ] {
        if let (Some(expected), Some(size)) = (expected, size) {
            let mut sink = io::sink();
            let mut output = Output {
                sink: &mut sink,
                digest: Sha256::new(),
                bytes: 0,
                maximum: size,
                notify: |_| Ok(()),
            };
            copy(
                afc,
                &format!("iTunes_Control/iTunes/{}", CATALOG_NAMES[index]),
                size,
                limit,
                &mut output,
                control,
            )
            .await?;
            check_control(control)?;
            if output.bytes != size || <[u8; 32]>::from(output.digest.finalize()) != expected {
                return Err(CatalogGuardDrift(
                    "live catalog bytes differ; partial audio evidence only",
                )
                .into());
            }
        }
    }
    let after = catalog_stats(afc, control).await?;
    catalog_sizes(&after)?;
    check_control(control)?;
    if before != after {
        return Err(CatalogGuardDrift("catalog changed during hash guard").into());
    }
    Ok(before)
}

// Validate every returned file before classifying a mismatch as catalogue drift.
// Malformed or oversized evidence never gains the initial-only retry type.
fn catalog_sizes(stats: &[Option<Attributes>]) -> Result<(u64, Option<u64>)> {
    let database = regular_size(stats[0].as_ref().context("live catalog absent")?)?;
    ensure!(
        (1..=music_protocol::catalogue::sqlite::MAX_DATABASE_BYTES).contains(&database),
        "live catalog database exceeds bound"
    );
    let wal = stats[1].as_ref().map(regular_size).transpose()?;
    ensure!(
        wal.is_none_or(|size| size <= music_protocol::catalogue::sqlite::MAX_WAL_BYTES),
        "live catalog WAL exceeds bound"
    );
    if let Some(attrs) = &stats[2] {
        ensure!(
            regular_size(attrs)? <= 1024 * 1024,
            "catalog SHM exceeds bound"
        );
    }
    Ok((database, wal))
}

struct Output<'a, W, F> {
    sink: &'a mut W,
    digest: Sha256,
    bytes: u64,
    maximum: u64,
    notify: F,
}
impl<W: Write, F: FnMut(u64) -> Result<()>> Write for Output<'_, W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.maximum - self.bytes {
            return Err(io::Error::other("index copy exceeds expected length"));
        }
        let count = self.sink.write(bytes)?;
        self.digest.update(&bytes[..count]);
        self.bytes += count as u64;
        (self.notify)(self.bytes).map_err(io::Error::other)?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

struct Spool {
    directory: PathBuf,
    file: PathBuf,
}
impl Spool {
    fn new(parent: &Path) -> Result<Self> {
        let directory = parent.join(format!(
            "audio-index-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_nanos(),
            SPOOL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&directory)
            .context("creating private audio index spool")?;
        Ok(Self {
            file: directory.join("audio.m4a"),
            directory,
        })
    }
    fn open(&self) -> Result<File> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(&self.file)?)
    }
    fn clear(&self) -> Result<()> {
        fs::remove_file(&self.file).context("unlinking this pass's local audio spool")
    }
    fn finish(&self) -> Result<()> {
        fs::remove_dir(&self.directory).context("removing this pass's empty local spool directory")
    }
}
impl Drop for Spool {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
        let _ = fs::remove_dir(&self.directory);
    }
}

use music_protocol::media::other_audio as other;
pub use other::OtherCodec;
pub(crate) use other::inspect as inspect_mpeg_frames;
