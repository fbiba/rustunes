//! Durable local evidence for one device/library. This module never contacts a
//! device and never authorizes deletion. Live identity, catalog, anchor, source,
//! and readback checks remain the caller's responsibility.
//!
//! Prepared -> Started -> Verified, or Prepared -> Cancelled. Reservations are
//! permanent even after cancellation. A Started attempt has no retry transition:
//! after interruption it requires fresh external reconciliation. SQLite EXTRA
//! synchronization plus directory fsync precedes every returned write permit.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use plist::Value;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};

use crate::engine::state::audio_index::IndexIdentity;
use music_protocol::catalogue::identity::LibraryIdentity;
use music_protocol::media::fingerprint::{EncodedAlac, SourceFingerprint};

const APPLICATION_ID: i64 = 0x49534a31;
/// The journal schema: every journal is created from it and must match it.
const SCHEMA: &str = include_str!("journal/schema.sql");

mod active;

mod rows;

mod bindings;
pub use active::{BindingRef, JournalHistorySummary};
pub use bindings::BindingId;
const MAX_OPERATION: usize = 4 * 1024 * 1024;
const MAX_CONTENT: usize = 512;
const DB_NAME: &str = "journal.sqlite";
const MAX_READ_ONLY_DATABASE: u64 = 64 * 1024 * 1024;
static READ_ONLY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// This journal holds a schema or digests of another rustunes version than
/// this build's, so its records cannot be compared with fresh captures. The
/// phone is not affected: `rustunes pull` builds a journal from the phone,
/// carrying forward what this one knew, and `sync` and `push` do that first.
#[derive(Debug)]
pub struct Superseded(&'static str);
impl std::fmt::Display for Superseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this journal was written by another version of rustunes ({}); run `rustunes pull` to carry it forward from the phone",
            self.0
        )
    }
}
impl std::error::Error for Superseded {}
/// Whether `error` comes from a journal [`Superseded`] by this build.
pub fn is_superseded(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Superseded>())
}

/// Hashes exact device identity and both persisted library identity components.
/// No opaque device/library values are included in diagnostics or Debug output.
pub struct Identity {
    device: [u8; 32],
    library: [u8; 32],
}

impl Identity {
    /// Reconstruct scope for reading a local journal against saved evidence.
    /// This does not attest to a live device; execution must use `new` with the
    /// freshly discovered device identifier and current preferences again.
    pub fn for_saved_device(
        device_identifier_sha256: [u8; 32],
        library: &LibraryIdentity,
    ) -> Result<Self> {
        nonzero_hash(&device_identifier_sha256)?;
        Ok(Self {
            device: device_identifier_sha256,
            library: library.binding_sha256(),
        })
    }

    pub fn device_digest(&self) -> [u8; 32] {
        self.device
    }

    pub fn library_digest(&self) -> [u8; 32] {
        self.library
    }

    /// The same two digests, as the identity a device audio index binds to.
    pub(crate) fn index_identity(&self) -> IndexIdentity {
        IndexIdentity {
            device_identifier_sha256: self.device,
            library_identity_sha256: self.library,
        }
    }

    pub fn new(device_identifier: &str, library: &LibraryIdentity) -> Result<Self> {
        Ok(Self {
            device: device_binding(device_identifier)?,
            library: library.binding_sha256(),
        })
    }
}

/// The digest a journal binds a phone's UDID to.
pub fn device_binding(device_identifier: &str) -> Result<[u8; 32]> {
    ensure!(
        !device_identifier.is_empty()
            && device_identifier.len() <= 256
            && device_identifier.bytes().all(|b| b.is_ascii_graphic()),
        "invalid device identity"
    );
    let mut hash = Sha256::new();
    hash.update(b"rustunes/device-binding\0");
    hash.update(device_identifier.as_bytes());
    Ok(hash.finalize().into())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttemptKind {
    Insert,
    Artwork,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttemptState {
    Prepared,
    ReconciliationRequired,
    Verified,
    Cancelled,
    /// A Started Insert whose metadata-only effect was removed again by a
    /// verified compensating removal batch. No binding exists.
    Compensated,
    /// A Started Insert or Artwork the phone never applied: a fresh
    /// observation found the anchor unmoved, nothing of it in the catalogue
    /// and nothing staged but its own unfinished pair, which was removed. Its
    /// reservations stay; nothing is resent.
    NotApplied,
    /// A Started Artwork whose session ended after the phone applied its
    /// metadata (the anchor moved by one, the track took the new cache ID)
    /// and before any cover upload completed, so no image arrived. No
    /// binding changes and its reservations stay; the next sync sends the
    /// cover again as an ordinary update.
    Abandoned,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttemptId(i64);

impl AttemptId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Artwork {
    pub cache_id: u32,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// Audio readback uses the full transferred file hash, independently of the
/// encoded-audio matching key. Collection scope is an opaque caller-defined
/// digest retained for future planning, never inferred from a path here.
#[derive(Clone, PartialEq, Eq)]
pub struct Content {
    /// Last transferred and fully read-back audio container. This is NOT the
    /// automatically refreshed fingerprint of the current desired source file.
    /// Tag/cover edits may change that source's full hash while the on-device
    /// audio file remains byte-for-byte unchanged. Artwork preserves this entire
    /// value and records its separate JPEG hash below; it never requires audio
    /// retransfer merely because desired source tags or cover bytes changed.
    pub source: music_protocol::media::fingerprint::TransferredSource,
    pub track_pid: u64,
    pub path: String,
    pub artwork: Option<Artwork>,
    pub collection: Option<[u8; 32]>,
}

pub struct PreparedAttempt {
    pub kind: AttemptKind,
    pub content: Content,
    /// Only newly allocated collections belong here; reused ones are not reserved.
    pub new_album_pid: Option<u64>,
    pub new_artist_pid: Option<u64>,
    pub current_media_anchor: u32,
    pub operation: Vec<u8>,
    /// Receipt binding fresh device/library/catalog checks and source inspection.
    pub input_receipt_sha256: [u8; 32],
}

pub struct Attempt {
    pub id: AttemptId,
    pub state: AttemptState,
    pub prepared: PreparedAttempt,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindingReceiptKind {
    InsertVerification,
    ArtworkVerification,
    /// The first version of a binding that an insert batch created.
    InsertBatchVerification,
    /// The first version of a binding a pull recorded from a fresh capture.
    PullVerification,
}
impl BindingReceiptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InsertVerification => "insert_verification",
            Self::ArtworkVerification => "artwork_verification",
            Self::InsertBatchVerification => "insert_batch_verification",
            Self::PullVerification => "pull_verification",
        }
    }
    fn from_number(n: i64) -> Result<Self> {
        Ok(match n {
            1 => Self::InsertVerification,
            2 => Self::ArtworkVerification,
            3 => Self::InsertBatchVerification,
            4 => Self::PullVerification,
            _ => bail!("invalid binding receipt kind"),
        })
    }
}
pub struct Binding {
    pub content: Content,
    pub receipt_kind: BindingReceiptKind,
    pub receipt_sha256: [u8; 32],
    pub media_anchor: u32,
}

pub struct StartEvidence {
    pub ready_media_anchor: u32,
    pub ready_receipt_sha256: [u8; 32],
    pub signature: [u8; 21],
}

/// Caller attests to fresh same-library catalog presence, complete audio
/// readback, and (when applicable) artwork readback/relationship verification.
/// A terminal protocol response alone is not sufficient evidence.
pub struct CompletionEvidence {
    pub track_pid: u64,
    pub path: String,
    pub audio_bytes: u64,
    pub audio_file_sha256: [u8; 32],
    pub artwork_sha256: Option<[u8; 32]>,
    pub media_anchor: u32,
    pub receipt_sha256: [u8; 32],
}

/// Deliberately non-Clone with private construction. Losing this value requires
/// reconciliation; reopening never issues another permit for a Started attempt.
pub struct StartedPermit {
    id: AttemptId,
    nonce: [u8; 32],
    operation: Vec<u8>,
    signature: [u8; 21],
}

impl StartedPermit {
    pub fn operation(&self) -> &[u8] {
        &self.operation
    }
    pub fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}
impl music_protocol::protocol::session::StartedOperation for StartedPermit {
    fn operation(&self) -> &[u8] {
        StartedPermit::operation(self)
    }
    fn signature(&self) -> &[u8; 21] {
        StartedPermit::signature(self)
    }
}

pub struct Journal {
    connection: Connection,
    directory: File,
    // Original validated namespace, retained even when SQLite reads a private clone.
    directory_path: PathBuf,
    database: File,
    nonce: [u8; 32],
    poisoned: bool,
    // Fields drop in declaration order: close SQLite before removing its copy.
    _read_only_copy: Option<std::sync::Arc<ReadOnlyCopy>>,
}

impl Journal {
    /// Stable lifetime identity for an active binding.
    #[cfg(test)]
    pub fn active_binding_id(
        &self,
        audio: &impl music_protocol::media::fingerprint::EncodedIdentity,
    ) -> Result<Option<BindingId>> {
        self.ordinary_inspectable()?;
        bindings::active_id(&self.connection, audio.encoded_sha256())
    }

    /// Rebind an already open journal to newly observed device preferences.
    pub fn ensure_identity(&self, identity: &Identity) -> Result<()> {
        self.ordinary_inspectable()?;
        let (device, library) = self.connection.query_row(
            "SELECT device,library FROM scope WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        ensure!(
            device == identity.device && library == identity.library,
            "journal does not belong to the observed device and library"
        );
        Ok(())
    }

    fn ordinary_inspectable(&self) -> Result<()> {
        self.usable()?;
        Ok(())
    }

    fn usable(&self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "journal durability failed; close and reopen for reconciliation"
        );
        Ok(())
    }
}

fn connect(path: &Path) -> Result<Connection> {
    connect_existing(path, false)
}

fn connect_existing(path: &Path, read_only: bool) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        (if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        }) | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(250))?;
    connection.pragma_update(None, "trusted_schema", false)?;
    connection.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    Ok(connection)
}

/// SQLite READ_ONLY can still create WAL/SHM files during a mode race. Only this
/// disposable namespace is ever supplied to SQLite for inspection; no immutable
/// assertion is made about the concurrently mutable original journal.
struct ReadOnlyCopy {
    directory: PathBuf,
    sha256: [u8; 32],
    source_metadata: fs::Metadata,
    /// Set once this copy's complete schema has been validated; its bytes
    /// never change while it is held.
    schema_checked: std::sync::OnceLock<()>,
}

impl ReadOnlyCopy {
    fn connect(&self) -> Result<Connection> {
        let connection = connect_existing(&self.directory.join(DB_NAME), true)?;
        connection.pragma_update(None, "query_only", true)?;
        let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        ensure!(
            mode == "delete",
            "read-only journal requires existing delete mode"
        );
        Ok(connection)
    }
}

impl Drop for ReadOnlyCopy {
    fn drop(&mut self) {
        // Remove only our known private copies, never original journal files.
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = fs::remove_file(self.directory.join(format!("{DB_NAME}{suffix}")));
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

fn read_only_bytes(source: &File, bytes: u64, output: &mut impl Write) -> Result<[u8; 32]> {
    ensure!(
        bytes <= MAX_READ_ONLY_DATABASE,
        "read-only journal exceeds size limit"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut offset = 0;
    while offset < bytes {
        let count = usize::try_from((bytes - offset).min(buffer.len() as u64))?;
        source.read_exact_at(&mut buffer[..count], offset)?;
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
        offset += count as u64;
    }
    let mut extra = [0];
    ensure!(
        source.read_at(&mut extra, bytes)? == 0,
        "journal grew during read-only opening"
    );
    Ok(hash.finalize().into())
}

fn require_quiescent_read_only(directory: &Path, database: &File) -> Result<()> {
    for suffix in ["-journal", "-wal", "-shm"] {
        match fs::symlink_metadata(directory.join(format!("{DB_NAME}{suffix}"))) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect journal recovery evidence"),
            Ok(_) => bail!("journal recovery sidecar exists; read-only access refuses recovery"),
        }
    }
    let mut header = [0; 20];
    std::os::unix::fs::FileExt::read_exact_at(database, &mut header, 0)
        .context("read journal database header")?;
    ensure!(
        &header[..16] == b"SQLite format 3\0" && header[18] == 1 && header[19] == 1,
        "read-only journal requires existing rollback mode"
    );
    Ok(())
}

fn configure(connection: &Connection) -> Result<()> {
    connection.pragma_update(None, "foreign_keys", true)?;
    let mode: String = connection.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    ensure!(mode == "delete", "journal requires rollback journal mode");
    connection.pragma_update(None, "synchronous", "EXTRA")?;
    ensure!(
        connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))? == 3,
        "journal requires EXTRA durability"
    );
    Ok(())
}

fn decode_reserved_path(bytes: &[u8]) -> Result<String> {
    let path = String::from_utf8(bytes.to_vec()).context("invalid reserved path encoding")?;
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4
            && parts[0] == "itunes_control"
            && parts[1] == "music"
            && parts[2].len() == 3
            && parts[2].starts_with('f')
            && parts[2][1..].bytes().all(|b| b.is_ascii_digit())
            && music_protocol::media::format::Format::EXTENSIONS
                .iter()
                .any(|e| parts[3].len() == 4 + e.len() && parts[3].ends_with(e))
            && parts[3][..4].bytes().all(|b| b.is_ascii_lowercase()),
        "invalid canonical reserved music path"
    );
    Ok(path)
}

/// Same conservative Music/FNN component shape used by the catalog index;
/// extension is deliberately not a codec classifier. Collision keys fold ASCII.
fn music_path(path: &str) -> Result<String> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "observed Music path exceeds bound"
    );
    let p: Vec<_> = path.split('/').collect();
    ensure!(
        p.len() == 4
            && p[0] == "iTunes_Control"
            && p[1] == "Music"
            && p[2].len() == 3
            && p[2].starts_with('F')
            && p[2][1..].bytes().all(|b| b.is_ascii_digit())
            && !p[3].is_empty()
            && p[3].len() <= 255
            && p[3] != "."
            && p[3] != ".."
            && p[3]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "unsafe observed Music path"
    );
    Ok(path.to_ascii_lowercase())
}

/// A removed row's path as a collision key: `music_path`, folded.
fn validate_path_key(value: &[u8]) -> Result<()> {
    let path = std::str::from_utf8(value)?;
    ensure!(
        path.len() <= 4096 && path == path.to_ascii_lowercase(),
        "invalid observed path collision key"
    );
    let parts: Vec<_> = path.split('/').collect();
    ensure!(
        parts.len() == 4
            && parts[0] == "itunes_control"
            && parts[1] == "music"
            && parts[2].starts_with('f'),
        "invalid observed path collision key"
    );
    ensure!(
        music_path(&format!(
            "iTunes_Control/Music/F{}/{}",
            &parts[2][1..],
            parts[3]
        ))? == path,
        "invalid observed path collision key"
    );
    Ok(())
}

fn durable_commit(
    transaction: Transaction<'_>,
    database: &File,
    directory: &File,
    poisoned: &mut bool,
) -> Result<()> {
    *poisoned = true; // Commit/fsync failure has an uncertain outcome; no permit.
    transaction.commit()?;
    database.sync_all()?;
    directory.sync_all()?;
    *poisoned = false;
    Ok(())
}

fn reserve(
    connection: &Connection,
    namespace: i64,
    value: &[u8],
    attempt: Option<AttemptId>,
    consumed: bool,
) -> Result<()> {
    reserve_for(
        connection,
        namespace,
        value,
        attempt.map(|id| id.0),
        None,
        consumed,
    )
}

/// Reserve for an attempt or for an insert batch (never both).
fn reserve_for(
    connection: &Connection,
    namespace: i64,
    value: &[u8],
    attempt: Option<i64>,
    insert_batch: Option<i64>,
    consumed: bool,
) -> Result<()> {
    let removed = match namespace {
        1 => Some("SELECT EXISTS(SELECT 1 FROM removal_targets WHERE track=?1)"),
        3 => {
            Some("SELECT EXISTS(SELECT 1 FROM removal_targets WHERE CAST(lower(path) AS BLOB)=?1)")
        }
        _ => None,
    };
    if let Some(sql) = removed {
        ensure!(
            !connection.query_row(sql, [value], |r| r.get::<_, bool>(0))?,
            "identity or destination belongs to a removed catalogue row"
        );
    }
    connection
        .execute(
            "INSERT INTO reservations(namespace,value,attempt,consumed,insert_batch) VALUES(?1,?2,?3,?4,?5)",
            params![namespace, value, attempt, consumed, insert_batch],
        )
        .context("identity or destination is already reserved")?;
    Ok(())
}

fn kind_number(kind: AttemptKind) -> i64 {
    match kind {
        AttemptKind::Insert => 1,
        AttemptKind::Artwork => 2,
    }
}

fn binding_query(suffix: &str) -> String {
    format!("SELECT audio,content,receipt,anchor,track,path,receipt_kind FROM bindings{suffix}")
}
struct RawBinding {
    audio: Vec<u8>,
    content: Vec<u8>,
    receipt: Vec<u8>,
    anchor: u32,
    track: Vec<u8>,
    path: String,
    receipt_kind: i64,
}

fn read_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawBinding> {
    Ok(RawBinding {
        audio: row.get(0)?,
        content: row.get(1)?,
        receipt: row.get(2)?,
        anchor: row.get(3)?,
        track: row.get(4)?,
        path: row.get(5)?,
        receipt_kind: row.get(6)?,
    })
}

fn verify_content(content: &Content, evidence: &CompletionEvidence) -> Result<()> {
    nonzero_hash(&evidence.receipt_sha256)?;
    ensure!(
        content.track_pid == evidence.track_pid
            && content.path == evidence.path
            && content.source.file_bytes == evidence.audio_bytes
            && content.source.full_file_sha256 == evidence.audio_file_sha256
            && content.artwork.as_ref().map(|art| art.sha256) == evidence.artwork_sha256,
        "verified content differs from the prepared binding"
    );
    Ok(())
}

fn nonzero_hash(hash: &[u8; 32]) -> Result<()> {
    ensure!(*hash != [0; 32], "missing evidence digest");
    Ok(())
}

fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid journal binary field length"))
}

/// A stored audio identity this build computes differently from its stored
/// components: another build's hash definitions wrote it.
const STALE_IDENTITY: &str = "its audio identities were hashed differently";

pub(crate) mod mpeg_insert;
