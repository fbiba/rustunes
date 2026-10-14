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
    pub(crate) async fn verify_state(&self, afc: &mut Transfer) -> Result<()> {
        state_guard(afc, self).await
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

/// New ordinary insert only. No attempt/reservation is created until the first
/// read-only guards succeed. The exact final input is published before prepare.
pub(crate) async fn execute_new_native_insert<F, Fut>(
    journal: &mut Journal,
    insertion: Insertion<'_>,
    input_path: &Path,
    input_bytes: &[u8],
    control: &Control,
    connect: F,
) -> Result<PendingVerification>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    ensure!(
        insertion.state.copied_cdb.is_some(),
        "production insertion requires bound copied CDB evidence"
    );
    ensure!(
        journal.outstanding()?.is_none(),
        "new insertion requires an idle journal"
    );
    ensure!(
        insertion.attempt.kind == AttemptKind::Insert,
        "deferred preparation is insertion-only"
    );
    ensure!(
        input_bytes.len() as u64 <= SMALL
            && <[u8; 32]>::from(Sha256::digest(input_bytes))
                == insertion.attempt.input_receipt_sha256,
        "deferred input differs from immutable insertion intent"
    );
    ensure!(
        !input_path.try_exists()?,
        "final insertion input already exists"
    );
    execute_insert_inner(
        journal,
        insertion,
        input_path,
        input_bytes,
        control,
        connect,
    )
    .await
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

fn finish_read_only_preflight(
    journal: &mut Journal,
    prepared: &PreparedAttempt,
    input_path: &Path,
    input_bytes: &[u8],
    control: &Control,
    guarded: Result<()>,
) -> Result<AttemptId> {
    control.check()?;
    if let Err(error) = guarded {
        return Err(match error.downcast::<StateReadDrift>() {
            Ok(drift) => PreSessionStateDrift { drift }.into(),
            Err(other) => other,
        });
    }
    durable_write(
        input_path.parent().context("input parent missing")?,
        input_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("invalid input name")?,
        input_bytes,
    )?;
    control.check()?;
    journal.prepare(copy_intent(prepared))
}

async fn execute_insert_inner<F, Fut>(
    journal: &mut Journal,
    insertion: Insertion<'_>,
    input_path: &Path,
    input_bytes: &[u8],
    control: &Control,
    connect: F,
) -> Result<PendingVerification>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    ensure!(
        control.deadline.duration_since(Instant::now()) <= MAX_DURATION,
        "insertion deadline exceeds 180-second bound"
    );
    let prepared = copy_intent(insertion.attempt);
    validate_preparation(&prepared, insertion.state)?;
    journal.ensure_identity(&Identity::new(
        &insertion.state.device,
        &insertion.state.identity,
    )?)?;
    let mut source = BoundSource::open(insertion.source, &prepared.content.source.require_alac()?)?;
    let asset_id = (prepared.content.track_pid as i64).to_string();
    let catalog_policy = insertion.state.ancillary.audio(&asset_id)?;
    catalog_policy.ensure_session(
        insertion.state.identity.library_id(),
        insertion.state.anchor,
    )?;
    let session_prepared = session::prepare(
        insertion.templates.request,
        &insertion.templates.finished,
        insertion.state.identity.library_id(),
        insertion.state.anchor,
    )?;
    let operations = operation_budget(insertion.state, prepared.content.source.file_bytes)?;
    create_trace(insertion.trace)?;
    durable_write(insertion.trace, "operation.plist", &prepared.operation)?;
    let cdb = insertion
        .state
        .copied_cdb
        .as_ref()
        .context("production profile lacks CDB binding")?;
    let policy = Value::Dictionary(
        [
            ("Format".to_owned(), "snapshot-bound-audio-ancillary".into()),
            (
                "BindingSHA256".to_owned(),
                Value::Data(insertion.state.ancillary.binding_sha256().to_vec()),
            ),
            (
                "MediaAnchor".to_owned(),
                u64::from(insertion.state.anchor).into(),
            ),
            ("OwnAssetID".to_owned(), asset_id.clone().into()),
            ("ForeignArtworkProvider".to_owned(), "Unavailable".into()),
            ("GeniusProvider".to_owned(), "Disabled".into()),
            (
                "CopiedCDBSHA256".to_owned(),
                Value::Data(cdb.sha256.to_vec()),
            ),
            ("CopiedCDBBytes".to_owned(), cdb.bytes.into()),
            (
                "SavedVoiceMemoAnchor".to_owned(),
                u64::from(music_protocol::protocol::templates::QUALIFIED_VOICE_MEMO_ANCHOR).into(),
            ),
            (
                "MaximumManifestAssets".to_owned(),
                (music_protocol::protocol::ancillary::MAX_MANIFEST_ASSETS as u64).into(),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let mut bytes = Vec::new();
    policy.to_writer_binary(&mut bytes)?;
    durable_write(insertion.trace, "ancillary-policy.plist", &bytes)?;
    let result = async {
        control.check()?;
        let provider = timeout_at(
            control
                .deadline
                .min(Instant::now() + Duration::from_secs(15)),
            connect(),
        )
        .await
        .context("insertion discovery deadline")??;
        ensure!(
            provider.udid == insertion.state.device,
            "discovered device differs from prepared state"
        );
        let mut afc = Transfer::connect_bounded(
            &provider,
            insertion.trace,
            control.deadline,
            operations,
            Arc::clone(&control.cancelled),
        )
        .await?;
        let stage = format!("Airlock/Media/{asset_id}.m4a");
        let bare = format!("Airlock/Media/{asset_id}");
        let destination = &prepared.content.path;
        let guarded = async {
            namespace_guard(&mut afc, destination, &stage, &bare).await?;
            state_guard(&mut afc, insertion.state).await?;
            source.check()?;
            control.check()
        }
        .await;
        if guarded.is_err() {
            // Drop the only connected transport before returning refresh authority.
            drop(afc);
            return finish_read_only_preflight(
                journal,
                &prepared,
                input_path,
                input_bytes,
                control,
                guarded,
            )
            .map(|_| unreachable!());
        }
        let attempt = finish_read_only_preflight(
            journal,
            &prepared,
            input_path,
            input_bytes,
            control,
            guarded,
        )?;
        control.check()?;
        let mut session = Session::connect(
            &provider,
            session_prepared,
            insertion.trace,
            control.deadline,
        )
        .await?;
        // Every mutable prerequisite is checked again in this exact Ready
        // session, before recording Started and before the first AFC write.
        state_guard(&mut afc, insertion.state).await?;
        namespace_guard(&mut afc, destination, &stage, &bare).await?;
        source.check()?;
        control.check()?;
        let signature = session.sign_metadata(&prepared.operation)?;
        durable_write(insertion.trace, "operation.cig", &signature)?;
        sync_trace(insertion.trace)?;
        let ready_sha256 = session.ready_receipt_sha256();
        let start = StartEvidence {
            ready_media_anchor: insertion.state.anchor,
            ready_receipt_sha256: ready_sha256,
            signature,
        };
        let permit = journal.mark_started(attempt, start)?;
        // No mutating AFC call is reachable above this durable transition.
        let operation_sha256: [u8; 32] = Sha256::digest(permit.operation()).into();
        session.stage_metadata(&mut afc, &permit).await?;
        control.check()?;
        session.finish_metadata().await?;
        session.wait_for_audio_with_policy(&catalog_policy).await?;
        source.check()?;
        source.file().rewind()?;
        let source_bytes = source.file_bytes();
        staging_folders(&mut afc, false).await?;
        let mut upload = ObservedUpload::new(
            source.file(),
            None,
            prepared.content.track_pid,
            source_bytes,
        );
        let uploaded = afc
            .write_audio_new(&stage, &mut upload, source_bytes)
            .await?;
        upload.finish()?;
        ensure!(
            uploaded == prepared.content.source.full_file_sha256,
            "uploaded source hash differs; partial stage retained"
        );
        source.check()?;
        afc.require_absent(destination).await?;
        // The write checked the stored size; the permanent file must be this same file.
        let staged = afc
            .rename_absent(&stage, &bare, source.file_bytes())
            .await?;
        crate::engine::state::audio_index::cache::uploaded(
            &Identity::new(&insertion.state.device, &insertion.state.identity)?.device_digest(),
            destination,
            &staged,
            &prepared.content.source,
        );
        afc.require_absent(destination).await?;
        control.check()?;
        session.complete_audio(&asset_id, destination).await?;
        session.wait_finished().await?;
        let terminal_sha256 = session.terminal_receipt_sha256()?;
        drop(session);
        let permanent = afc
            .stat(destination)
            .await?
            .context("permanent audio disappeared")?;
        ensure!(
            same_file(&staged, &permanent),
            "permanent audio differs from the upload"
        );
        source.check()?;
        control.check()?;
        drop(afc);
        let mut evidence = TransferEvidence {
            content: prepared.content.clone(),
            operation_sha256,
            ready_receipt_sha256: ready_sha256,
            terminal_receipt_sha256: terminal_sha256,
            // The permanent file is the stage these bytes were written to.
            permanent_audio_sha256: uploaded,
            media_anchor: insertion
                .state
                .anchor
                .checked_add(1)
                .context("Media anchor exhausted")?,
            receipt_sha256: [0; 32],
        };
        let receipt = evidence_plist(&evidence)?;
        evidence.receipt_sha256 = Sha256::digest(&receipt).into();
        durable_write(insertion.trace, "pending-verification.plist", &receipt)?;
        sync_trace(insertion.trace)?;
        Ok(PendingVerification { permit, evidence })
    }
    .await;
    if let Err(error) = &result {
        // A logging failure never changes the original transport/journal outcome.
        let _ = durable_write(insertion.trace, "error.txt", format!("{error:#}\nNo retry or cleanup was attempted. Inspect the journal and reconcile Started state.\n").as_bytes());
    }
    result
}

fn validate_preparation(prepared: &PreparedAttempt, state: &FreshState) -> Result<()> {
    let content = &prepared.content;
    ensure!(
        content.artwork.is_none(),
        "insertion engine does not combine artwork transfer"
    );
    ensure!(
        content.source.file_bytes > 0 && content.source.file_bytes <= MAX_AUDIO,
        "source exceeds bounded audio profile"
    );
    ensure!(
        prepared.current_media_anchor == state.anchor
            && prepared.input_receipt_sha256 == state.input_receipt_sha256,
        "preparation and fresh-state evidence differ"
    );
    for id in [
        Some(content.track_pid),
        prepared.new_album_pid,
        prepared.new_artist_pid,
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            !state.occupied.contains(&id),
            "new identity collides with current catalog"
        );
    }
    session::validate_music_path(&content.path)?;
    ensure!(
        !state
            .paths
            .iter()
            .any(|path| path.eq_ignore_ascii_case(&content.path)),
        "destination is already referenced by current catalog"
    );
    check_database_prefix(&prepared.operation, state)
}

/// The single-insert preparation checks for every member of an insert batch:
/// bounded ALAC or MP3 sources, identities and destinations free in this exact state
/// and distinct within the batch, and the operation's database identities.
/// Each `insert_track` names exactly its member's cover, or none.
pub(crate) fn validate_batch<'a>(
    state: &FreshState,
    operation: &[u8],
    members: impl IntoIterator<Item = (&'a Content, [Option<u64>; 2])>,
) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut caches = BTreeSet::new();
    let inserts = batch_inserts(operation)?;
    for (content, new) in members {
        let item = inserts
            .get(&content.track_pid)
            .context("insert batch operation does not insert a member")?;
        let (cache, checksum) = (
            item.get("artwork_cache_id")
                .map(|v| v.as_unsigned_integer().context("invalid artwork cache ID"))
                .transpose()?,
            item.get("artwork_checksum")
                .and_then(Value::as_unsigned_integer)
                .context("insert has no artwork checksum")?,
        );
        // The native checksum counts the picture and its container bytes: an
        // MP4 cover's whole `data` atom (16 header bytes and the image), an
        // ID3 or FLAC cover's image payload alone.
        let header = if content.source.format().mp4() { 16 } else { 0 };
        match &content.artwork {
            Some(art) => ensure!(
                cache == Some(u64::from(art.cache_id))
                    && art.cache_id >= 100
                    && caches.insert(art.cache_id)
                    && Some(checksum) == art.bytes.checked_add(header + 1),
                "insert batch cover differs from its operation"
            ),
            None => ensure!(
                cache.is_none() && checksum == 0,
                "insert batch operation names a cover its member lacks"
            ),
        }
        ensure!(
            content.source.file_bytes > 0 && content.source.file_bytes <= MAX_AUDIO,
            "source exceeds bounded audio profile"
        );
        for id in std::iter::once(content.track_pid).chain(new.into_iter().flatten()) {
            ensure!(
                !state.occupied.contains(&id) && ids.insert(id),
                "new identity collides with the current catalog or another batch member"
            );
        }
        session::validate_format_music_path(&content.path, content.source.format())?;
        ensure!(
            !state
                .paths
                .iter()
                .any(|path| path.eq_ignore_ascii_case(&content.path))
                && paths.insert(content.path.to_ascii_lowercase()),
            "destination is already referenced by the current catalog or another batch member"
        );
    }
    ensure!(!ids.is_empty(), "insert batch has no members");
    ensure!(
        inserts.len() == paths.len(),
        "insert batch operation inserts a track outside its members"
    );
    check_database_prefix(operation, state)
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

fn check_database_prefix(operation: &[u8], state: &FreshState) -> Result<()> {
    let operation = Value::from_reader(Cursor::new(operation))?;
    let db = operation
        .as_dictionary()
        .and_then(|d| d.get("operations"))
        .and_then(Value::as_array)
        .and_then(|ops| ops.first())
        .and_then(Value::as_dictionary)
        .context("missing prepared database prefix")?;
    ensure!(
        db.get("pid")
            .and_then(Value::as_signed_integer)
            .map(|v| v as u64)
            == Some(state.database_pid)
            && db
                .get("db_info")
                .and_then(Value::as_dictionary)
                .and_then(|d| d.get("primary_container_pid"))
                .and_then(Value::as_signed_integer)
                .map(|v| v as u64)
                == Some(state.container_pid),
        "prepared database identities differ from current catalog"
    );
    Ok(())
}

fn operation_budget(state: &FreshState, source_bytes: u64) -> Result<usize> {
    ensure!(
        source_bytes <= MAX_AUDIO,
        "audio operation budget exceeds bound"
    );
    let state_chunks = state.state_read_chunks()?;
    // Two full state guards + audio writes (counted three times, from when
    // uploads were read back twice) + fixed metadata, attributes, opens/closes,
    // namespace and rename allowance.
    let operations = 512_u64
        .checked_add(
            state_chunks
                .checked_mul(2)
                .context("state budget overflow")?,
        )
        .and_then(|n| n.checked_add(source_bytes.div_ceil(CHUNK) * 3))
        .context("AFC operation budget overflow")?;
    let operations = usize::try_from(operations)?;
    ensure!(
        operations <= MAX_BOUNDED_OPS,
        "derived AFC operation allowance exceeds bound"
    );
    Ok(operations)
}

async fn namespace_guard(
    afc: &mut Transfer,
    destination: &str,
    stage: &str,
    bare: &str,
) -> Result<()> {
    let folder = destination
        .rsplit_once('/')
        .context("missing destination folder")?
        .0;
    for directory in ["iTunes_Control/Sync/Media", folder] {
        afc.require_directory(directory).await?;
    }
    afc.require_directory_or_absent(STAGING_MEDIA).await?;
    let names = afc.names("iTunes_Control/Sync/Media").await?;
    ensure!(
        names.iter().all(|name| name == "." || name == ".."),
        "metadata namespace contains leftovers; nothing removed or overwritten"
    );
    for path in [METADATA, SIGNATURE, stage, bare, destination] {
        afc.require_absent(path).await?;
    }
    Ok(())
}

/// Both guards of a single insert for a whole batch: unchanged state files,
/// an empty metadata namespace, and no stage, bare name or permanent file for
/// any member. `members` holds (stage, bare, destination) per member.
pub(crate) async fn batch_guard(
    afc: &mut Transfer,
    state: &FreshState,
    members: &[(String, String, String)],
) -> Result<()> {
    // The checks every member shared (folders, the empty metadata namespace,
    // no metadata or signature file) bracket the per-member names instead of
    // repeating for each of them.
    let folders = members
        .iter()
        .map(|(_, _, destination)| {
            Ok(destination
                .rsplit_once('/')
                .context("missing destination folder")?
                .0)
        })
        .collect::<Result<BTreeSet<_>>>()?;
    shared_namespace_guard(afc, &folders).await?;
    for (stage, bare, destination) in members {
        for path in [stage, bare, destination] {
            afc.require_absent(path).await?;
        }
    }
    shared_namespace_guard(afc, &folders).await?;
    state_guard(afc, state).await
}
async fn shared_namespace_guard(afc: &mut Transfer, folders: &BTreeSet<&str>) -> Result<()> {
    afc.require_directory("iTunes_Control/Sync/Media").await?;
    for folder in folders {
        afc.require_directory(folder).await?;
    }
    afc.require_directory_or_absent(STAGING_MEDIA).await?;
    let names = afc.names("iTunes_Control/Sync/Media").await?;
    ensure!(
        names.iter().all(|name| name == "." || name == ".."),
        "metadata namespace contains leftovers; nothing removed or overwritten"
    );
    for path in [METADATA, SIGNATURE] {
        afc.require_absent(path).await?;
    }
    Ok(())
}

/// One connection's allowance for two batch guards plus the metadata and
/// signature writes and readbacks.
pub(crate) fn batch_guard_budget(
    state: &FreshState,
    members: usize,
    operation_bytes: usize,
) -> Result<usize> {
    ensure!(
        operation_bytes > 0
            && operation_bytes <= music_protocol::protocol::signing::MAX_OPERATION_BYTES,
        "insert batch operation exceeds bound"
    );
    // Per member and guard: three directories, one listing, five stats.
    let operations = state
        .state_read_chunks()?
        .checked_mul(2)
        .and_then(|n| n.checked_add(u64::try_from(members).ok()?.checked_mul(2 * 16)?))
        .and_then(|n| n.checked_add((operation_bytes as u64).div_ceil(CHUNK) * 4 + 512))
        .context("insert batch guard budget overflow")?;
    let operations = usize::try_from(operations)?;
    ensure!(
        operations <= MAX_SESSION_OPS,
        "insert batch guard allowance exceeds bound"
    );
    Ok(operations)
}

/// One member's upload connection: write, stage readback, rename.
/// A new track's audio as an upload reads it, reported as
/// `Event::Uploading` before each read: the upload sends what it read before
/// it reads more.
pub(crate) struct ObservedUpload<'a> {
    file: &'a mut File,
    source: Option<&'a str>,
    track: u64,
    file_bytes: u64,
    read: u64,
}

impl<'a> ObservedUpload<'a> {
    pub(crate) fn new(
        file: &'a mut File,
        source: Option<&'a str>,
        track: u64,
        file_bytes: u64,
    ) -> Self {
        Self {
            file,
            source,
            track,
            file_bytes,
            read: 0,
        }
    }
    fn report(&self, sent: u64) -> Result<()> {
        crate::engine::observe(crate::engine::Event::Uploading {
            source: self.source,
            track: self.track,
            sent,
            file_bytes: self.file_bytes,
        })
    }
    /// The upload returned: all of the audio is on the phone.
    pub(crate) fn finish(self) -> Result<()> {
        self.report(self.file_bytes)
    }
}

impl std::io::Read for ObservedUpload<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        // A report that fails stops nothing: the upload is under way.
        let _ = self.report(self.read);
        let read = std::io::Read::read(self.file, buffer)?;
        self.read += read as u64;
        Ok(read)
    }
}

pub(crate) fn upload_budget(source_bytes: u64) -> Result<usize> {
    ensure!(
        source_bytes > 0 && source_bytes <= MAX_AUDIO,
        "audio upload exceeds bound"
    );
    let operations = usize::try_from(source_bytes.div_ceil(CHUNK) * 2 + 64)?;
    ensure!(
        operations <= MAX_BOUNDED_OPS,
        "audio upload allowance exceeds bound"
    );
    Ok(operations)
}

async fn state_guard(afc: &mut Transfer, state: &FreshState) -> Result<()> {
    let cdb = state
        .copied_cdb
        .as_ref()
        .context("state has no bound copied CDB")?;
    let mut before = Vec::new();
    for name in STATE_NAMES {
        before.push(afc.stat(&format!("iTunes_Control/iTunes/{name}")).await?);
    }
    for (index, expected) in state.files.iter().enumerate() {
        let path = format!("iTunes_Control/iTunes/{}", STATE_NAMES[index]);
        if let Some(expected) = expected {
            guard_size(
                before[index]
                    .as_ref()
                    .context("required state file is absent")?,
                expected,
                STATE_NAMES[index],
                if index < 2 { SMALL } else { MAX_CATALOG },
            )?;
            let hash = afc
                .read_hash(
                    &path,
                    expected.bytes,
                    if index < 2 { SMALL } else { MAX_CATALOG },
                )
                .await?;
            guard_hash(hash, expected, STATE_NAMES[index])?;
        } else {
            ensure!(
                before[index].is_none(),
                "live WAL presence differs from prepared state"
            );
        }
    }
    guard_size(
        before[5].as_ref().context("bound CDB is absent")?,
        cdb,
        STATE_NAMES[5],
        MAX_CATALOG,
    )?;
    let hash = afc
        .read_hash("iTunes_Control/iTunes/iTunesCDB", cdb.bytes, MAX_CATALOG)
        .await?;
    guard_hash(hash, cdb, STATE_NAMES[5])?;
    for (index, name) in STATE_NAMES.iter().enumerate() {
        let after = afc.stat(&format!("iTunes_Control/iTunes/{name}")).await?;
        ensure!(
            stable_attrs(&before[index]) == stable_attrs(&after),
            "state file attributes changed across readback"
        );
    }
    Ok(())
}

fn guard_size(
    attrs: &Attributes,
    expected: &ExpectedFile,
    file: &'static str,
    cap: u64,
) -> Result<()> {
    // Validate the full supported stat shape BEFORE classifying a mismatch.
    // Absence, nonregular objects, malformed/oversized attributes stay ordinary
    // refusal errors and can never be mistaken for refreshable drift.
    let actual: u64 = attrs
        .get("st_size")
        .context("missing state size")?
        .parse()?;
    regular_size(attrs, actual)?;
    let _: u64 = attrs
        .get("st_mtime")
        .context("missing state modification time")?
        .parse()?;
    ensure!(actual <= cap, "observed state file exceeds bound");
    if actual != expected.bytes {
        return Err(StateReadDrift {
            file,
            reason: "regular file size changed",
        }
        .into());
    }
    Ok(())
}
fn guard_hash(actual: [u8; 32], expected: &ExpectedFile, file: &'static str) -> Result<()> {
    if actual != expected.sha256 {
        return Err(StateReadDrift {
            file,
            reason: "complete file digest changed",
        }
        .into());
    }
    Ok(())
}

fn stable_attrs(attrs: &Option<Attributes>) -> Option<BTreeMap<&str, &str>> {
    attrs.as_ref().map(|attrs| {
        attrs
            .iter()
            .filter(|(key, _)| key.as_str() != "st_atime")
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    })
}

pub(crate) use music_protocol::media::held_source::BoundSource;
pub(crate) use music_protocol::protocol::transfer::{
    STAGING_ARTWORK, STAGING_MEDIA, staging_folders,
};

pub(crate) fn create_trace(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .context("insertion trace must be a new private directory")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()?;
    File::open(path)?.sync_all()?;
    Ok(())
}
pub(crate) fn durable_write(trace: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(trace.join(name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(trace)?.sync_all()?;
    Ok(())
}
pub(crate) fn sync_trace(trace: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(trace)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
        "trace directory changed"
    );
    for entry in fs::read_dir(trace)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(metadata.is_file(), "unexpected nonregular trace entry");
        File::open(path)?.sync_all()?;
    }
    File::open(trace)?.sync_all()?;
    Ok(())
}
fn evidence_plist(evidence: &TransferEvidence) -> Result<Vec<u8>> {
    let mut value = Dictionary::new();
    value.insert(
        "Status".into(),
        "PendingIndependentCatalogVerification".into(),
    );
    value.insert(
        "TrackPID".into(),
        (evidence.content.track_pid as i64).into(),
    );
    value.insert("PermanentPath".into(), evidence.content.path.clone().into());
    value.insert(
        "AudioBytes".into(),
        evidence.content.source.file_bytes.into(),
    );
    value.insert("MediaAnchor".into(), evidence.media_anchor.into());
    for (name, hash) in [
        ("OperationSHA256", evidence.operation_sha256),
        ("ReadyFrameSHA256", evidence.ready_receipt_sha256),
        ("TerminalFrameSHA256", evidence.terminal_receipt_sha256),
        ("PermanentAudioSHA256", evidence.permanent_audio_sha256),
        ("EncodedAudioSHA256", evidence.content.source.audio.sha256),
    ] {
        value.insert(name.into(), hex(&hash).into());
    }
    let mut bytes = Vec::new();
    Value::Dictionary(value).to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
use std::path::PathBuf;
