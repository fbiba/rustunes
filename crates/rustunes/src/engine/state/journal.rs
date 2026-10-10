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

mod coordination;
mod rows;
pub use coordination::{
    BatchCounts, BatchId, BatchRecord, BatchState, CoordinationId, CoordinationKind,
    CoordinationRecord, PlaylistTarget, RemovalIntent, RemovalTarget, StartedBatch,
};
mod insertion_batch;
pub use insertion_batch::{
    Conversion, InsertBatchId, InsertBatchRecord, InsertBatchState, InsertIntent, InsertTarget,
    TargetOutcome,
};
mod metadata_batch;
pub use metadata_batch::{
    MetadataBatchId, MetadataBatchRecord, MetadataBatchState, MetadataIntent, StartedMetadataBatch,
};

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

impl Binding {
    /// Digest of the entire last verified transferred Content, not desired tags.
    pub fn content_sha256(&self) -> Result<[u8; 32]> {
        let bytes = encode_content(&self.content)?;
        let mut h = Sha256::new();
        h.update(b"rustunes/binding-content\0");
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(bytes);
        Ok(h.finalize().into())
    }
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

    /// Active verified content for this audio.
    pub fn lookup_active_verified(
        &self,
        audio: &impl music_protocol::media::fingerprint::EncodedIdentity,
    ) -> Result<Option<Binding>> {
        self.ordinary_inspectable()?;
        lookup(&self.connection, audio.encoded_sha256())
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

    /// Record the absolute source path a verified single insertion sent, so a
    /// journal pulled later keeps it. A binding that records one keeps it.
    pub fn record_source(&mut self, track_pid: u64, source: &Path) -> Result<()> {
        self.ordinary_usable()?;
        ensure!(
            self._read_only_copy.is_none(),
            "read-only journal cannot record a source"
        );
        let source = source.to_str().context("source path is not UTF-8")?;
        ensure!(
            source.starts_with('/') && (2..=4096).contains(&source.len()),
            "source path must be absolute"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bound = tx.execute(
            "INSERT OR IGNORE INTO binding_sources SELECT r.id,?2 FROM active_bindings a JOIN binding_records r ON r.id=a.binding_id WHERE r.track=?1",
            params![track_pid.to_be_bytes(), source],
        )?;
        ensure!(bound <= 1, "track is bound more than once");
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }

    pub fn attempt(&self, id: AttemptId) -> Result<Attempt> {
        self.ordinary_inspectable()?;
        load_attempt(&self.connection, id)
    }

    /// Read the durable transition evidence; never recreates a write permit.
    pub fn start_evidence(&self, id: AttemptId) -> Result<Option<StartEvidence>> {
        self.ordinary_inspectable()?;
        let attempt = load_attempt(&self.connection, id)?;
        let (ready, signature): (Option<Vec<u8>>, Option<Vec<u8>>) = self.connection.query_row(
            "SELECT ready_receipt,signature FROM attempts WHERE id=?1",
            [id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        match (ready, signature) {
            (None, None)
                if matches!(
                    attempt.state,
                    AttemptState::Prepared | AttemptState::Cancelled
                ) =>
            {
                Ok(None)
            }
            (Some(ready), Some(signature))
                if matches!(
                    attempt.state,
                    AttemptState::ReconciliationRequired
                        | AttemptState::Verified
                        | AttemptState::NotApplied
                        | AttemptState::Abandoned
                ) =>
            {
                let ready_receipt_sha256 = array(&ready)?;
                let signature: [u8; 21] = array(&signature)?;
                nonzero_hash(&ready_receipt_sha256)?;
                ensure!(signature[0] == 1, "invalid stored signature envelope");
                Ok(Some(StartEvidence {
                    ready_media_anchor: attempt.prepared.current_media_anchor,
                    ready_receipt_sha256,
                    signature,
                }))
            }
            _ => anyhow::bail!("stored start evidence disagrees with attempt state"),
        }
    }

    pub fn cancel_prepared(&mut self, id: AttemptId) -> Result<()> {
        self.ordinary_usable()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            transaction.execute(
                "UPDATE attempts SET state=3 WHERE id=?1 AND state=0",
                [id.0]
            )? == 1,
            "only an unstarted Prepared attempt may be cancelled"
        );
        durable_commit(
            transaction,
            &self.database,
            &self.directory,
            &mut self.poisoned,
        )
    }

    pub fn mark_started(
        &mut self,
        id: AttemptId,
        evidence: StartEvidence,
    ) -> Result<StartedPermit> {
        self.ordinary_usable()?;
        nonzero_hash(&evidence.ready_receipt_sha256)?;
        ensure!(
            evidence.signature[0] == 1,
            "unsupported metadata signature envelope"
        );
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = load_attempt(&transaction, id)?;
        attempt.prepared.content.source.require_alac()?;
        ensure!(
            attempt.state == AttemptState::Prepared,
            "attempt already started or closed; reconcile instead of retrying"
        );
        ensure!(
            evidence.ready_media_anchor == attempt.prepared.current_media_anchor,
            "fresh Ready anchor differs from preparation"
        );
        transaction.execute(
            "UPDATE attempts SET state=1,ready_receipt=?1,signature=?2 WHERE id=?3",
            params![
                evidence.ready_receipt_sha256,
                evidence.signature.as_slice(),
                id.0
            ],
        )?;
        bindings::database_bound(&transaction, 4 * 1024 * 1024)?;
        durable_commit(
            transaction,
            &self.database,
            &self.directory,
            &mut self.poisoned,
        )?;
        Ok(StartedPermit {
            id,
            nonce: self.nonce,
            operation: attempt.prepared.operation,
            signature: evidence.signature,
        })
    }

    /// Consumes the only permit even if verification fails. A failed completion
    /// must be investigated with fresh reconciliation, never retransmitted.
    pub fn complete(&mut self, permit: StartedPermit, evidence: CompletionEvidence) -> Result<()> {
        self.ordinary_usable()?;
        ensure!(
            permit.nonce == self.nonce,
            "write permit belongs to another journal"
        );
        self.finish(permit.id, evidence, false)
    }

    /// The receipt must bind fresh identity/catalog and complete file readback.
    /// For the exact prepared revision, recovery may use the original durable
    /// Ready/signature/terminal proof with fresh SQL/prefs/CDB and preservation
    /// checks; it need not open another ATC session. Later revisions require
    /// separately established reconciliation. Absent or ambiguous content leaves
    /// Started unresolved and blocks further attempts; this never issues a retry.
    pub fn reconcile_completed(&mut self, id: AttemptId, fresh: CompletionEvidence) -> Result<()> {
        self.ordinary_usable()?;
        self.finish(id, fresh, true)
    }

    fn finish(
        &mut self,
        id: AttemptId,
        evidence: CompletionEvidence,
        reconciled: bool,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = load_attempt(&transaction, id)?;
        attempt.prepared.content.source.require_alac()?; // Storage decoding is not start/completion authority.
        ensure!(
            attempt.state == AttemptState::ReconciliationRequired,
            "only Started attempts may be verified"
        );
        verify_content(&attempt.prepared.content, &evidence)?;
        let expected_anchor = attempt.prepared.current_media_anchor + 1;
        ensure!(
            if reconciled {
                evidence.media_anchor >= expected_anchor
            } else {
                evidence.media_anchor == expected_anchor
            },
            "verification anchor does not cover prepared revision"
        );
        bindings::store_binding(
            &transaction,
            &attempt.prepared.content,
            &evidence,
            attempt.prepared.kind == AttemptKind::Artwork,
            id,
        )?;
        transaction.execute(
            "UPDATE reservations SET consumed=1 WHERE attempt=?1",
            [id.0],
        )?;
        transaction.execute(
            "UPDATE attempts SET state=2,verified_receipt=?1,verified_anchor=?2 WHERE id=?3",
            params![evidence.receipt_sha256, evidence.media_anchor, id.0],
        )?;
        bindings::database_bound(&transaction, 0)?;
        durable_commit(
            transaction,
            &self.database,
            &self.directory,
            &mut self.poisoned,
        )
    }

    /// Settle a Started Insert or Artwork the phone never applied. `receipt` is
    /// the digest of the retained observation proving it; reservations stay
    /// unconsumed and no binding changes. An Active coordination's attempt
    /// needs its permit, as every other write does.
    pub fn mark_not_applied(&mut self, id: AttemptId, receipt: [u8; 32]) -> Result<()> {
        self.ordinary_usable()?;
        nonzero_hash(&receipt)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            transaction.execute(
                "UPDATE attempts SET state=5,verified_receipt=?1 WHERE id=?2 AND state=1 AND kind IN(1,2)",
                params![receipt, id.0],
            )? == 1,
            "only a Started Insert or Artwork may be settled not applied"
        );
        durable_commit(
            transaction,
            &self.database,
            &self.directory,
            &mut self.poisoned,
        )
    }

    /// Settle a Started Artwork whose session the host abandoned after the
    /// phone applied its metadata at `anchor`, with no cover arrived.
    /// `receipt` is the digest of the retained observation proving it;
    /// reservations stay unconsumed and no binding changes.
    pub fn mark_abandoned(&mut self, id: AttemptId, receipt: [u8; 32], anchor: u32) -> Result<()> {
        self.ordinary_usable()?;
        nonzero_hash(&receipt)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            transaction.execute(
                "UPDATE attempts SET state=6,verified_receipt=?1,verified_anchor=?2 WHERE id=?3 AND state=1 AND kind=2 AND ?2>base_anchor",
                params![receipt, anchor, id.0],
            )? == 1,
            "only a Started Artwork may be settled abandoned, at an anchor past its own"
        );
        durable_commit(
            transaction,
            &self.database,
            &self.directory,
            &mut self.poisoned,
        )
    }

    fn ordinary_inspectable(&self) -> Result<()> {
        self.usable()?;
        Ok(())
    }

    fn ordinary_usable(&self) -> Result<()> {
        self.ordinary_inspectable()?;
        coordination::ensure_no_route_owner(&self.connection)
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

fn ensure_idle(connection: &Connection) -> Result<()> {
    ensure!(
        !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE state IN (0,1))",
            [],
            |row| row.get::<_, bool>(0)
        )?,
        "an outstanding attempt requires completion, cancellation, or reconciliation"
    );
    coordination::ensure_no_outstanding_batch(connection)
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

fn load_attempt(connection: &Connection, id: AttemptId) -> Result<Attempt> {
    let raw = connection.query_row("SELECT kind,state,content,operation,operation_sha,album,artist,base_anchor,input_receipt FROM attempts WHERE id=?1", [id.0], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, Vec<u8>>(2)?, row.get::<_, Vec<u8>>(3)?, row.get::<_, Vec<u8>>(4)?, row.get::<_, Option<Vec<u8>>>(5)?, row.get::<_, Option<Vec<u8>>>(6)?, row.get::<_, u32>(7)?, row.get::<_, Vec<u8>>(8)?))
    }).context("journal attempt not found")?;
    let kind = match raw.0 {
        1 => AttemptKind::Insert,
        2 => AttemptKind::Artwork,
        _ => bail!("invalid journal attempt kind"),
    };
    let state = match raw.1 {
        0 => AttemptState::Prepared,
        1 => AttemptState::ReconciliationRequired,
        2 => AttemptState::Verified,
        3 => AttemptState::Cancelled,
        4 => AttemptState::Compensated,
        5 => AttemptState::NotApplied,
        6 => AttemptState::Abandoned,
        _ => bail!("invalid journal attempt state"),
    };
    let actual_sha: [u8; 32] = Sha256::digest(&raw.3).into();
    ensure!(
        actual_sha == array::<32>(&raw.4)?,
        "prepared operation digest differs"
    );
    let prepared = PreparedAttempt {
        kind,
        content: decode_content(&raw.2)?,
        operation: raw.3,
        new_album_pid: raw
            .5
            .map(|b| Ok::<_, anyhow::Error>(u64::from_be_bytes(array(&b)?)))
            .transpose()?,
        new_artist_pid: raw
            .6
            .map(|b| Ok::<_, anyhow::Error>(u64::from_be_bytes(array(&b)?)))
            .transpose()?,
        current_media_anchor: raw.7,
        input_receipt_sha256: array(&raw.8)?,
    };
    validate_prepared(&prepared)?;
    Ok(Attempt {
        id,
        state,
        prepared,
    })
}

fn lookup(connection: &Connection, audio: &[u8; 32]) -> Result<Option<Binding>> {
    let raw = connection
        .query_row(&binding_query(" WHERE audio=?1"), [audio], read_binding)
        .optional()?;
    raw.map(decode_binding).transpose()
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

fn decode_binding(raw: RawBinding) -> Result<Binding> {
    let content = decode_content(&raw.content)?;
    ensure!(
        content.source.audio.sha256.as_slice() == raw.audio
            && content.track_pid.to_be_bytes().as_slice() == raw.track
            && content.path == raw.path,
        "journal binding key differs from content"
    );
    Ok(Binding {
        content,
        receipt_sha256: array(&raw.receipt)?,
        receipt_kind: BindingReceiptKind::from_number(raw.receipt_kind)?,
        media_anchor: raw.anchor,
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

fn validate_prepared(prepared: &PreparedAttempt) -> Result<()> {
    validate_content(&prepared.content)?;
    ensure!(
        prepared.content.source.is_alac() || prepared.kind == AttemptKind::Insert,
        "MPEG artwork intent is not qualified"
    );
    nonzero_hash(&prepared.input_receipt_sha256)?;
    let revision = prepared
        .current_media_anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    ensure!(
        !prepared.operation.is_empty() && prepared.operation.len() <= MAX_OPERATION,
        "prepared operation exceeds bounds"
    );
    let mut ids = BTreeSet::from([prepared.content.track_pid]);
    for id in [prepared.new_album_pid, prepared.new_artist_pid]
        .into_iter()
        .flatten()
    {
        ensure!(
            id != 0 && ids.insert(id),
            "new identities must be nonzero and distinct"
        );
    }
    // A narrow command/target guard prevents opaque prepared bytes from turning
    // this Insert/Artwork API into a deletion or multi-track write permit. The
    // protocol builder remains responsible for all other metadata semantics.
    let value = Value::from_reader(Cursor::new(&prepared.operation))
        .context("invalid prepared operation plist")?;
    let root = value
        .as_dictionary()
        .context("operation envelope is not a dictionary")?;
    ensure!(
        root.len() == 3
            && root.get("timestamp").and_then(Value::as_date).is_some()
            && root.get("revision").and_then(Value::as_unsigned_integer)
                == Some(u64::from(revision)),
        "operation revision or envelope differs"
    );
    let operations = root
        .get("operations")
        .and_then(Value::as_array)
        .context("operation list missing")?;
    ensure!(
        operations.len() == 2,
        "journal only accepts one track operation plus unchanged database information"
    );
    let first = operations[0]
        .as_dictionary()
        .context("invalid database operation")?;
    ensure!(
        first.len() == 3
            && first.get("operation").and_then(Value::as_string) == Some("update_db_info")
            && first
                .get("pid")
                .and_then(Value::as_signed_integer)
                .is_some_and(|id| id != 0)
            && first
                .get("db_info")
                .and_then(Value::as_dictionary)
                .is_some_and(|d| d.len() == 1
                    && d.get("primary_container_pid")
                        .and_then(Value::as_signed_integer)
                        .is_some_and(|id| id != 0)),
        "unsupported database operation"
    );
    let own = operations[1]
        .as_dictionary()
        .context("invalid track operation")?;
    ensure!(
        own.get("pid")
            .and_then(Value::as_signed_integer)
            .map(|id| id as u64)
            == Some(prepared.content.track_pid),
        "operation targets another track"
    );
    let item = own
        .get("item")
        .and_then(Value::as_dictionary)
        .context("track item missing")?;
    match prepared.kind {
        AttemptKind::Insert => {
            ensure!(
                own.get("operation").and_then(Value::as_string) == Some("insert_track")
                    && prepared.content.artwork.is_none(),
                "unsupported insertion operation"
            );
            for (key, id) in [
                ("album_pid", prepared.new_album_pid),
                ("artist_pid", prepared.new_artist_pid),
            ] {
                if let Some(id) = id {
                    ensure!(
                        item.get(key)
                            .and_then(Value::as_signed_integer)
                            .map(|pid| pid as u64)
                            == Some(id),
                        "reserved collection differs from operation"
                    );
                }
            }
        }
        AttemptKind::Artwork => {
            let art = prepared
                .content
                .artwork
                .as_ref()
                .context("artwork operation has no artwork")?;
            ensure!(
                prepared.new_album_pid.is_none()
                    && prepared.new_artist_pid.is_none()
                    && own.len() == 3
                    && own.get("operation").and_then(Value::as_string) == Some("update_track")
                    && item.len() == 2
                    && item
                        .get("artwork_cache_id")
                        .and_then(Value::as_unsigned_integer)
                        == Some(u64::from(art.cache_id))
                    && item
                        .get("artwork_checksum")
                        .and_then(Value::as_unsigned_integer)
                        == art.bytes.checked_add(17),
                "unsupported artwork-only operation"
            );
        }
    }
    Ok(())
}

fn validate_content(content: &Content) -> Result<()> {
    ensure!(content.track_pid != 0, "invalid content track identity");
    content.source.validate()?;
    let extension = content.source.format().extension();
    let parts: Vec<_> = content.path.split('/').collect();
    ensure!(
        parts.len() == 4
            && parts[0] == "iTunes_Control"
            && parts[1] == "Music"
            && parts[2].len() == 3
            && parts[2].starts_with('F')
            && parts[2][1..].bytes().all(|b| b.is_ascii_digit())
            && parts[3].len() == 4 + extension.len()
            && parts[3].ends_with(extension)
            && parts[3][..4].bytes().all(|b| b.is_ascii_alphabetic()),
        "unsupported permanent music path"
    );
    if let Some(art) = &content.artwork {
        ensure!(
            art.cache_id >= 100 && (4..=32 * 1024 * 1024).contains(&art.bytes),
            "invalid artwork reservation"
        );
        nonzero_hash(&art.sha256)?;
    }
    if let Some(collection) = content.collection {
        nonzero_hash(&collection)?;
    }
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

// Local fixed-field record led by its codec kind. No phone protocol is encoded here.
fn encode_content(content: &Content) -> Result<Vec<u8>> {
    validate_content(content)?;
    let source = &content.source;
    let mut bytes;
    if let Some(audio) = source.audio.encoded() {
        bytes = vec![3];
        bytes.extend(source.file_bytes.to_be_bytes());
        bytes.extend(source.full_file_sha256);
        bytes.extend(source.audio.sha256);
        bytes.extend(audio.to_bytes());
    } else if let Some(audio) = source.audio.mpeg() {
        bytes = vec![2];
        bytes.extend(source.file_bytes.to_be_bytes());
        bytes.extend(source.full_file_sha256);
        bytes.extend(source.audio.sha256);
        bytes.extend(audio.sample_rate.to_be_bytes());
        bytes.push(audio.channels);
        bytes.extend(audio.frame_count.to_be_bytes());
        bytes.extend(audio.audio_offset.to_be_bytes());
        bytes.extend(audio.audio_bytes.to_be_bytes());
        bytes.extend(audio.trailing_bytes.to_be_bytes());
        bytes.extend(audio.frame_bytes_sha256);
    } else {
        let audio = source.audio.alac()?;
        bytes = vec![1];
        bytes.extend(source.file_bytes.to_be_bytes());
        for hash in [
            &source.full_file_sha256,
            &audio.sha256,
            &audio.alac_config_sha256,
            &audio.sample_sizes_sha256,
            &audio.media_sha256,
        ] {
            bytes.extend(hash);
        }
        bytes.extend(audio.timescale.to_be_bytes());
        bytes.extend(audio.duration_ticks.to_be_bytes());
        bytes.extend(audio.media_bytes.to_be_bytes());
    }
    bytes.extend(content.track_pid.to_be_bytes());
    bytes.extend(u16::try_from(content.path.len())?.to_be_bytes());
    bytes.extend(content.path.as_bytes());
    bytes.push(u8::from(content.artwork.is_some()));
    if let Some(art) = &content.artwork {
        bytes.extend(art.cache_id.to_be_bytes());
        bytes.extend(art.bytes.to_be_bytes());
        bytes.extend(art.sha256);
    }
    bytes.push(u8::from(content.collection.is_some()));
    if let Some(collection) = &content.collection {
        bytes.extend(collection);
    }
    ensure!(
        bytes.len() <= MAX_CONTENT,
        "content record exceeds journal bounds"
    );
    Ok(bytes)
}

/// A stored audio identity this build computes differently from its stored
/// components: another build's hash definitions wrote it.
const STALE_IDENTITY: &str = "its audio identities were hashed differently";

fn decode_content(bytes: &[u8]) -> Result<Content> {
    decode_content_as(bytes, true)
}

/// `decode_content`; with `stored_identity` false, a stored audio identity
/// this build computes differently is replaced by this build's, from the
/// stored components (a predecessor's content, `carry`), instead of refused.
fn decode_content_as(bytes: &[u8], stored_identity: bool) -> Result<Content> {
    ensure!(
        bytes.len() <= MAX_CONTENT,
        "content record exceeds journal bounds"
    );
    fn take<const N: usize>(reader: &mut Cursor<&[u8]>) -> Result<[u8; N]> {
        let mut value = [0; N];
        reader
            .read_exact(&mut value)
            .context("truncated journal content")?;
        Ok(value)
    }
    fn flag(reader: &mut Cursor<&[u8]>) -> Result<bool> {
        let byte = take::<1>(reader)?[0];
        ensure!(byte <= 1, "invalid journal content flag");
        Ok(byte == 1)
    }
    let mut reader = Cursor::new(bytes);
    // The codec kind: 1 ALAC, 2 MPEG-1 Layer III, 3 any other format.
    let kind = take::<1>(&mut reader)?[0];
    ensure!(matches!(kind, 1..=3), "unknown content record kind");
    let file_bytes = u64::from_be_bytes(take(&mut reader)?);
    let full_file_sha256 = take(&mut reader)?;
    let sha256 = take(&mut reader)?;
    let source = if kind == 1 {
        let mut audio = EncodedAlac {
            sha256,
            alac_config_sha256: take(&mut reader)?,
            sample_sizes_sha256: take(&mut reader)?,
            media_sha256: take(&mut reader)?,
            timescale: u32::from_be_bytes(take(&mut reader)?),
            duration_ticks: u64::from_be_bytes(take(&mut reader)?),
            media_bytes: u64::from_be_bytes(take(&mut reader)?),
        };
        if audio.composite_sha256() != sha256 {
            if stored_identity {
                return Err(Superseded(STALE_IDENTITY).into());
            }
            audio.sha256 = audio.composite_sha256();
        }
        SourceFingerprint {
            file_bytes,
            full_file_sha256,
            audio,
        }
        .into()
    } else if kind == 3 {
        let mut audio = [0; music_protocol::media::fingerprint::EncodedAudio::BYTES];
        reader
            .read_exact(&mut audio)
            .context("truncated journal content")?;
        let value = music_protocol::media::fingerprint::TransferredSource::encoded(
            file_bytes,
            full_file_sha256,
            music_protocol::media::fingerprint::EncodedAudio::from_bytes(&audio)?,
        )?;
        if stored_identity && value.audio.sha256 != sha256 {
            return Err(Superseded(STALE_IDENTITY).into());
        }
        value
    } else {
        let value = music_protocol::media::fingerprint::TransferredSource::mpeg(
            file_bytes,
            full_file_sha256,
            music_protocol::media::fingerprint::EncodedMpeg1Layer3 {
                sample_rate: u32::from_be_bytes(take(&mut reader)?),
                channels: take::<1>(&mut reader)?[0],
                frame_count: u32::from_be_bytes(take(&mut reader)?),
                audio_offset: u64::from_be_bytes(take(&mut reader)?),
                audio_bytes: u64::from_be_bytes(take(&mut reader)?),
                trailing_bytes: u64::from_be_bytes(take(&mut reader)?),
                frame_bytes_sha256: take(&mut reader)?,
            },
        )?;
        if stored_identity && value.audio.sha256 != sha256 {
            return Err(Superseded(STALE_IDENTITY).into());
        }
        value
    };
    let track_pid = u64::from_be_bytes(take(&mut reader)?);
    let length = usize::from(u16::from_be_bytes(take(&mut reader)?));
    ensure!(length <= 128, "journal path exceeds bounds");
    let mut path = vec![0; length];
    reader.read_exact(&mut path)?;
    let path = String::from_utf8(path).context("invalid journal path encoding")?;
    let artwork = if flag(&mut reader)? {
        Some(Artwork {
            cache_id: u32::from_be_bytes(take(&mut reader)?),
            bytes: u64::from_be_bytes(take(&mut reader)?),
            sha256: take(&mut reader)?,
        })
    } else {
        None
    };
    let collection = if flag(&mut reader)? {
        Some(take(&mut reader)?)
    } else {
        None
    };
    ensure!(
        reader.position() == bytes.len() as u64,
        "trailing journal content bytes"
    );
    let content = Content {
        source,
        track_pid,
        path,
        artwork,
        collection,
    };
    validate_content(&content)?;
    Ok(content)
}

pub(crate) mod mpeg_insert;
