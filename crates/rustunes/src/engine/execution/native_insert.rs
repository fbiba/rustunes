//! One journaled native insertion, with no retry, overwrite, reset, or deletion.
//!
//! Matching/allocation belong to the planner. This engine consumes an immutable
//! Prepared insertion and fresh identity/catalog evidence. Success deliberately
//! leaves Started: independent post-catalog verification must consume the permit
//! with Journal::complete. A terminal and an audio hash alone are not that proof.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Cursor, Seek, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::engine::execution::device::DeviceProvider;
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, timeout_at};

use crate::engine::state::journal::{
    AttemptId, AttemptKind, Content, Identity, Journal, PreparedAttempt, StartEvidence,
    StartedPermit,
};
use music_protocol::catalogue::identity::{self, LibraryIdentity};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::ancillary::EligibleSnapshot;
use music_protocol::protocol::session::{self, Session};
use music_protocol::protocol::transfer::{
    Attributes, MAX_AUDIO, MAX_BOUNDED_OPS, MAX_CATALOG, MAX_SESSION_OPS, METADATA, SIGNATURE,
    Transfer, hex, regular_size, same_file,
};

const SMALL: u64 = 1024 * 1024;
const MAX_DURATION: Duration = Duration::from_secs(180);
const CHUNK: u64 = 64 * 1024;
const STATE_NAMES: [&str; 6] = [
    "iTunesPrefs",
    "iTunesPrefs.plist",
    "MediaLibrary.sqlitedb",
    "MediaLibrary.sqlitedb-wal",
    "MediaLibrary.sqlitedb-shm",
    "iTunesCDB",
];

struct ExpectedFile {
    bytes: u64,
    sha256: [u8; 32],
}

/// Constructed from a caller's stable, WAL-aware capture and exact preference
/// copies. The input receipt must bind these observations and the complete
/// matching/allocation decision; its bytes are retained durably by the caller.
pub struct FreshState {
    device: String,
    identity: LibraryIdentity,
    files: [Option<ExpectedFile>; 4],
    copied_cdb: Option<ExpectedFile>,
    anchor: u32,
    database_pid: u64,
    container_pid: u64,
    occupied: BTreeSet<u64>,
    paths: BTreeSet<String>,
    input_receipt_sha256: [u8; 32],
    ancillary: EligibleSnapshot,
}

impl FreshState {
    pub(crate) fn device_identifier(&self) -> &str {
        &self.device
    }
    pub(crate) fn library_identity(&self) -> &LibraryIdentity {
        &self.identity
    }
    pub(crate) fn media_anchor(&self) -> u32 {
        self.anchor
    }
    pub(crate) fn input_receipt_sha256(&self) -> [u8; 32] {
        self.input_receipt_sha256
    }
    pub(crate) fn ancillary(&self) -> &EligibleSnapshot {
        &self.ancillary
    }
    pub(crate) fn ensure_catalog(&self, catalog: &Catalog) -> Result<()> {
        self.ancillary.ensure_current(catalog, &self.identity)
    }
    pub(crate) fn state_read_chunks(&self) -> Result<u64> {
        self.files
            .iter()
            .flatten()
            .chain(self.copied_cdb.iter())
            .try_fold(0_u64, |sum, file| {
                ensure!(
                    file.bytes <= MAX_CATALOG,
                    "state operation budget exceeds bound"
                );
                sum.checked_add(file.bytes.div_ceil(CHUNK))
                    .context("state chunk budget overflow")
            })
    }

    /// Bind the exact stable CDB copy used to qualify the runtime profile.
    /// Its saved VoiceMemo must remain 1 and its database PID must match this
    /// state's current SQL catalog. Both pre/post-Ready guards then read back
    /// these exact bytes. Saved CDB Media never overrides the current anchor.
    pub fn bind_copied_cdb(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            self.copied_cdb.is_none(),
            "CDB evidence already bound; construct a fresh state to change it"
        );
        let saved = music_protocol::catalogue::anchor::parse(bytes)?;
        ensure!(
            saved.database_pid() != 0 && saved.database_pid() == self.database_pid,
            "copied CDB and current SQL database identities disagree"
        );
        ensure!(
            saved.saved_voice_memo_anchor()
                == music_protocol::protocol::templates::QUALIFIED_VOICE_MEMO_ANCHOR,
            "copied CDB has an unqualified saved VoiceMemo anchor"
        );
        self.copied_cdb = Some(ExpectedFile {
            bytes: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        });
        Ok(())
    }

    pub fn new(
        device: &str,
        catalog: &Catalog,
        raw_preferences: &[u8],
        plist_preferences: &[u8],
        input_receipt_sha256: [u8; 32],
    ) -> Result<Self> {
        let paths = catalog.music_paths()?;
        Self::with_paths(
            device,
            catalog,
            raw_preferences,
            plist_preferences,
            input_receipt_sha256,
            paths,
        )
    }

    /// The state binding when exactly `unfilled` (rows an unfinished insertion
    /// left) have no file location; every other row's path is bound. With no
    /// such rows this is `new`.
    pub(crate) fn for_unfilled(
        device: &str,
        catalog: &Catalog,
        raw_preferences: &[u8],
        plist_preferences: &[u8],
        input_receipt_sha256: [u8; 32],
        unfilled: &BTreeSet<u64>,
    ) -> Result<Self> {
        if unfilled.is_empty() {
            return Self::new(
                device,
                catalog,
                raw_preferences,
                plist_preferences,
                input_receipt_sha256,
            );
        }
        let mut paths = BTreeSet::new();
        let mut pending = BTreeSet::new();
        for track in catalog.tracks() {
            match &track.afc_path {
                Some(path) => {
                    ensure!(paths.insert(path.clone()), "duplicate catalog track path");
                }
                None => {
                    pending.insert(track.sync_id);
                }
            }
        }
        ensure!(
            pending == *unfilled,
            "only the rows an unfinished insertion left may lack a file location"
        );
        Self::with_paths(
            device,
            catalog,
            raw_preferences,
            plist_preferences,
            input_receipt_sha256,
            paths,
        )
    }

    fn with_paths(
        device: &str,
        catalog: &Catalog,
        raw_preferences: &[u8],
        plist_preferences: &[u8],
        input_receipt_sha256: [u8; 32],
        paths: BTreeSet<String>,
    ) -> Result<Self> {
        ensure!(
            input_receipt_sha256 != [0; 32],
            "missing current-state/matching receipt"
        );
        let identity = identity::parse(raw_preferences)?;
        identity.ensure_matches(&identity::parse(plist_preferences)?)?;
        Identity::new(device, &identity)?;
        let small = |bytes: &[u8]| -> Result<Option<ExpectedFile>> {
            ensure!(
                !bytes.is_empty() && bytes.len() as u64 <= SMALL,
                "preferences exceed bounded state input"
            );
            Ok(Some(ExpectedFile {
                bytes: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
            }))
        };
        let hashes = catalog.source_hashes();
        ensure!(
            hashes.database_bytes > 0 && hashes.database_bytes <= MAX_CATALOG,
            "catalog database exceeds insertion state bound"
        );
        let wal = match (hashes.wal, hashes.wal_bytes) {
            (Some(sha256), Some(bytes)) if bytes <= MAX_CATALOG => {
                Some(ExpectedFile { bytes, sha256 })
            }
            (None, None) => None,
            _ => anyhow::bail!(
                "catalog WAL exceeds insertion state bound or has inconsistent presence"
            ),
        };
        Ok(Self {
            device: device.to_owned(),
            ancillary: EligibleSnapshot::new(catalog, &identity),
            identity,
            files: [
                small(raw_preferences)?,
                small(plist_preferences)?,
                Some(ExpectedFile {
                    bytes: hashes.database_bytes,
                    sha256: hashes.database,
                }),
                wal,
            ],
            copied_cdb: None,
            anchor: catalog.persisted_media_anchor(),
            database_pid: catalog.database_pid(),
            container_pid: catalog.primary_container_pid(),
            occupied: catalog.represented_sync_ids().clone(),
            paths,
            input_receipt_sha256,
        })
    }
}

pub struct Insertion<'a> {
    pub attempt: &'a PreparedAttempt,
    pub source: &'a Path,
    pub state: &'a FreshState,
    pub templates: music_protocol::protocol::templates::Templates,
    /// Must not exist. All partial evidence is retained, including on error.
    pub trace: &'a Path,
}

pub struct Control {
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}

impl Control {
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            "insertion cancelled; reconcile any Started attempt"
        );
        ensure!(
            Instant::now() < self.deadline,
            "original insertion deadline expired"
        );
        Ok(())
    }
}

pub struct TransferEvidence {
    pub content: Content,
    pub operation_sha256: [u8; 32],
    pub ready_receipt_sha256: [u8; 32],
    pub terminal_receipt_sha256: [u8; 32],
    pub permanent_audio_sha256: [u8; 32],
    pub media_anchor: u32,
    pub receipt_sha256: [u8; 32],
}

/// No completion or ownership is inferred by returning this value. The caller
/// checks a new independently captured catalog and full preservation invariants,
/// then consumes permit with Journal::complete. Dropping it requires reconciliation.
pub struct PendingVerification {
    pub permit: StartedPermit,
    pub evidence: TransferEvidence,
}

#[derive(Debug)]
struct StateReadDrift {
    file: &'static str,
    reason: &'static str,
}
impl std::fmt::Display for StateReadDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state read drift in {}: {}", self.file, self.reason)
    }
}
impl std::error::Error for StateReadDrift {}

/// Only the deferred engine's closed, read-only pre-session branch constructs
/// this error. A Ready-session or unknown failure cannot grant refresh authority.
#[derive(Debug)]
pub(crate) struct PreSessionStateDrift {
    drift: StateReadDrift,
}
impl std::fmt::Display for PreSessionStateDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "read-only pre-session {}; no attempt prepared",
            self.drift
        )
    }
}
impl std::error::Error for PreSessionStateDrift {}

#[cfg(test)]
impl PreSessionStateDrift {
    pub(crate) fn fixture() -> Self {
        Self {
            drift: StateReadDrift {
                file: "MediaLibrary.sqlitedb-wal",
                reason: "synthetic observed drift",
            },
        }
    }
}

fn copy_intent(p: &PreparedAttempt) -> PreparedAttempt {
    PreparedAttempt {
        kind: p.kind,
        content: p.content.clone(),
        new_album_pid: p.new_album_pid,
        new_artist_pid: p.new_artist_pid,
        current_media_anchor: p.current_media_anchor,
        operation: p.operation.clone(),
        input_receipt_sha256: p.input_receipt_sha256,
    }
}

/// Each `insert_track` item of a batch operation by its track PID.
fn batch_inserts(operation: &[u8]) -> Result<BTreeMap<u64, Dictionary>> {
    let operation = Value::from_reader(Cursor::new(operation))?;
    let operations = operation
        .as_dictionary()
        .and_then(|d| d.get("operations"))
        .and_then(Value::as_array)
        .context("insert batch operation has no operations")?;
    let mut inserts = BTreeMap::new();
    for op in operations.iter().skip(1) {
        let op = op
            .as_dictionary()
            .filter(|op| op.get("operation").and_then(Value::as_string) == Some("insert_track"))
            .context("insert batch carries an operation other than insert_track")?;
        let pid = op
            .get("pid")
            .and_then(Value::as_signed_integer)
            .context("insert has no track PID")? as u64;
        let item = op
            .get("item")
            .and_then(Value::as_dictionary)
            .context("insert has no item")?;
        ensure!(
            inserts.insert(pid, item.clone()).is_none(),
            "insert batch operation inserts a track twice"
        );
    }
    Ok(inserts)
}
