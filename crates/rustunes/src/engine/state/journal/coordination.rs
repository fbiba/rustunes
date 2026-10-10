//! Coordinated operations and batched removals.
//!
//! A coordination owns the journal while Active: ordinary writes refuse unless
//! this process holds the coordination's permit. A removal batch is one signed
//! metadata operation that removes several catalogue rows; it is journaled
//! Prepared -> Started -> Verified (or Cancelled, or NotApplied after a fresh
//! observation proves a sent batch did not apply). Verification retires every
//! owned target in one transaction.
use super::*;
use std::sync::Mutex;

const MAX_INTENT: usize = 16 * 1024 * 1024;
const MAX_OUTCOME: usize = 1024 * 1024;
const MAX_COORDINATIONS: i64 = 10_000;
pub(super) const MAX_BATCHES: i64 = 10_000;
pub(super) const MAX_TARGETS: usize = 50_000;
const MAX_TARGET_FILE: u64 = 512 * 1024 * 1024;
const OUTSTANDING_BATCH: &str = "an outstanding removal batch requires settlement";

/// What a coordination coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinationKind {
    /// A whole Clear.
    Clear = 1,
    /// A library reset and re-add.
    ReplaceLibrary = 2,
    /// One track's changed-audio replacement.
    ReplaceTrack = 3,
}
impl CoordinationKind {
    fn decode(n: i64) -> Result<Self> {
        Ok(match n {
            1 => Self::Clear,
            2 => Self::ReplaceLibrary,
            3 => Self::ReplaceTrack,
            _ => bail!("invalid coordination kind"),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinationState {
    Active,
    Completed,
}
impl CoordinationState {
    fn decode(n: i64) -> Result<Self> {
        Ok(match n {
            0 => Self::Active,
            1 => Self::Completed,
            _ => bail!("invalid coordination state"),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoordinationId(pub(super) i64);
impl CoordinationId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}
/// Stored data only; reopening grants no permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinationRecord {
    pub id: CoordinationId,
    pub kind: CoordinationKind,
    pub state: CoordinationState,
    pub phase: u8,
    pub revision: u64,
    pub intent: Vec<u8>,
    pub intent_sha256: [u8; 32],
    pub evidence: PathBuf,
    pub base_anchor: u32,
    pub outcome: Option<Vec<u8>>,
}
/// One ordered link to the native work a coordination performed; exactly one
/// of the four is present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoordinationStep {
    pub step: u32,
    pub batch: Option<BatchId>,
    pub attempt: Option<AttemptId>,
    pub insert_batch: Option<InsertBatchId>,
    pub metadata_batch: Option<MetadataBatchId>,
}
/// The native work one coordination step links to.
pub(super) enum Work {
    Removal(BatchId),
    Attempt(AttemptId),
    Insert(InsertBatchId),
    Metadata(MetadataBatchId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BatchId(pub(super) i64);
impl BatchId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchState {
    Prepared,
    Started,
    Verified,
    /// Never sent.
    Cancelled,
    /// Sent, then proven unapplied by a fresh observation.
    NotApplied,
}
impl BatchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Started => "started",
            Self::Verified => "verified",
            Self::Cancelled => "cancelled",
            Self::NotApplied => "not_applied",
        }
    }
    fn decode(n: i64) -> Result<Self> {
        Ok(match n {
            0 => Self::Prepared,
            1 => Self::Started,
            2 => Self::Verified,
            3 => Self::Cancelled,
            4 => Self::NotApplied,
            _ => bail!("invalid removal batch state"),
        })
    }
}
/// One catalogue row a batch removes. `path`, `file_bytes` and `file_sha256`
/// are all absent for a locationless row. `binding` names the owned active
/// binding version that the removal retires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalTarget {
    pub track: u64,
    pub item_pid: i64,
    pub path: Option<String>,
    pub file_bytes: Option<u64>,
    pub file_sha256: Option<[u8; 32]>,
    pub binding: Option<(BindingId, u32)>,
}
/// One ordinary playlist a batch deletes: the container's sync id,
/// which the operation's `delete_playlist` names, and its name when prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistTarget {
    pub container: u64,
    pub name: String,
}
/// Fresh observations bound into the Prepared row. The operation is the exact
/// signed metadata operation, so its timestamp is fixed at preparation.
#[derive(Clone, Debug)]
pub struct RemovalIntent {
    /// The Active coordination this batch is the next step of, if any.
    pub coordination: Option<CoordinationId>,
    /// A Started Insert whose locationless row this batch removes.
    pub compensates: Option<AttemptId>,
    /// A Partial insert batch whose locationless rows this batch removes;
    /// never together with `compensates`.
    pub compensates_insert_batch: Option<InsertBatchId>,
    pub operation: Vec<u8>,
    pub base_anchor: u32,
    pub before_current: [u8; 32],
    pub before_index: [u8; 32],
    pub input_receipt: [u8; 32],
    /// Absolute run directory holding the saved Before evidence.
    pub evidence: PathBuf,
    /// Tracks to remove, or `playlists`; exactly one is nonempty.
    pub targets: Vec<RemovalTarget>,
    pub playlists: Vec<PlaylistTarget>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRecord {
    pub id: BatchId,
    pub coordination: Option<CoordinationId>,
    pub compensates: Option<AttemptId>,
    pub compensates_insert_batch: Option<InsertBatchId>,
    /// The insert batch whose additions this batch's removals share one
    /// session with (`Journal::prepare_session`); its operation, receipts and
    /// evidence are the session's.
    pub with_insert_batch: Option<InsertBatchId>,
    pub state: BatchState,
    pub operation: Vec<u8>,
    pub base_anchor: u32,
    pub before_current: [u8; 32],
    pub before_index: [u8; 32],
    pub input_receipt: [u8; 32],
    pub evidence: PathBuf,
    pub ready_receipt: Option<[u8; 32]>,
    pub signature: Option<[u8; 21]>,
    pub result_receipt: Option<[u8; 32]>,
    pub verified_anchor: Option<u32>,
    pub targets: Vec<RemovalTarget>,
    pub playlists: Vec<PlaylistTarget>,
}
/// Batches by state, for status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchCounts {
    pub prepared: u64,
    pub started: u64,
    pub verified: u64,
    pub cancelled: u64,
    pub not_applied: u64,
}

/// Returned only after the durable Started commit. Not Clone: one send. A lost
/// value requires fresh reconciliation, never another permit.
pub struct StartedBatch {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedBatch {
    pub fn operation(&self) -> &[u8] {
        &self.operation
    }
    pub fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}

/// The one writer an Active coordination admits: this process, for this
/// journal, until the permit drops. Ordinary journal writes opened by the
/// coordinator pass the ownership check while it is held.
pub struct CoordinationPermit(());
impl Drop for CoordinationPermit {
    fn drop(&mut self) {
        *permit_slot() = None;
    }
}
/// (journal nonce, coordination id) of the process-wide permit.
static PERMIT: Mutex<Option<([u8; 32], i64)>> = Mutex::new(None);
fn permit_slot() -> std::sync::MutexGuard<'static, Option<([u8; 32], i64)>> {
    PERMIT.lock().unwrap_or_else(|e| e.into_inner())
}
/// Whether this process holds the permit for coordination `id` of the journal
/// behind `c`.
pub(super) fn permit_held(c: &Connection, id: i64) -> Result<bool> {
    let nonce: Vec<u8> = c.query_row("SELECT nonce FROM scope WHERE singleton=1", [], |r| {
        r.get(0)
    })?;
    Ok(*permit_slot() == Some((array(&nonce)?, id)))
}

/// Refuse while a coordination owns the journal. The process that entered the
/// Active coordination (`Journal::enter_coordination`) is the one writer the
/// coordination admits.
pub(super) fn ensure_no_route_owner(c: &Connection) -> Result<()> {
    match active_id(c)? {
        Some(id) if permit_held(c, id)? => Ok(()),
        Some(id) => bail!(
            "coordination {id} owns the journal until it completes; status shows the command that continues it"
        ),
        None => Ok(()),
    }
}

pub(super) fn active_id(c: &Connection) -> Result<Option<i64>> {
    Ok(
        c.query_row("SELECT id FROM coordinations WHERE state=0", [], |r| {
            r.get(0)
        })
        .optional()?,
    )
}

fn outstanding_batch_id(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT id FROM removal_batches WHERE state IN(0,1)",
        [],
        |r| r.get(0),
    )
    .optional()?)
}

/// Idle checks for batches: no outstanding removal, insert or metadata batch.
pub(super) fn ensure_no_outstanding_batch(c: &Connection) -> Result<()> {
    ensure!(outstanding_batch_id(c)?.is_none(), OUTSTANDING_BATCH);
    insertion_batch::ensure_no_outstanding_batch(c)
}

/// Native work that blocks starting or completing a coordination.
pub(super) fn ensure_quiescent(c: &Connection) -> Result<()> {
    ensure_idle(c)
}

pub(super) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(super) fn evidence_text(path: &Path) -> Result<&str> {
    let text = path.to_str().context("evidence path is not UTF-8")?;
    ensure!(
        path.is_absolute() && (2..=4096).contains(&text.len()) && !text.contains('\0'),
        "evidence must be an absolute path of at most 4096 bytes"
    );
    Ok(text)
}

fn load_coordination(c: &Connection, id: CoordinationId) -> Result<CoordinationRecord> {
    type Raw = (
        i64,
        i64,
        i64,
        i64,
        Vec<u8>,
        Vec<u8>,
        String,
        i64,
        Option<Vec<u8>>,
    );
    let raw: Raw = c
        .query_row(
            "SELECT kind,state,phase,revision,intent,intent_sha,evidence,base_anchor,outcome FROM coordinations WHERE id=?1",
            [id.0],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .optional()?
        .context("coordination not found")?;
    let intent_sha256: [u8; 32] = array(&raw.5)?;
    ensure!(
        digest(&raw.4) == intent_sha256,
        "coordination intent digest differs"
    );
    ensure!(
        Path::new(&raw.6).is_absolute(),
        "coordination evidence path is not absolute"
    );
    Ok(CoordinationRecord {
        id,
        kind: CoordinationKind::decode(raw.0)?,
        state: CoordinationState::decode(raw.1)?,
        phase: u8::try_from(raw.2).context("coordination phase")?,
        revision: u64::try_from(raw.3).context("coordination revision")?,
        intent: raw.4,
        intent_sha256,
        evidence: PathBuf::from(raw.6),
        base_anchor: u32::try_from(raw.7).context("coordination anchor")?,
        outcome: raw.8,
    })
}

fn validate_target(target: &RemovalTarget) -> Result<()> {
    ensure!(
        target.track != 0 && target.item_pid != 0,
        "removal target identities must be nonzero"
    );
    match (&target.path, target.file_bytes, &target.file_sha256) {
        (None, None, None) => {}
        (Some(path), Some(bytes), Some(sha)) => {
            music_path(path)?;
            ensure!(
                (1..=MAX_TARGET_FILE).contains(&bytes),
                "removal target file size"
            );
            nonzero_hash(sha)?;
        }
        _ => bail!("removal target path, size and digest are all present or all absent"),
    }
    if let Some((id, version)) = target.binding {
        ensure!(
            id.as_i64() > 0 && version > 0,
            "invalid removal target binding"
        );
    }
    Ok(())
}

fn validate_intent(intent: &RemovalIntent) -> Result<()> {
    ensure!(
        !intent.operation.is_empty() && intent.operation.len() <= MAX_OPERATION,
        "removal operation exceeds bounds"
    );
    ensure!(
        intent.base_anchor < u32::MAX,
        "removal base anchor exhausted"
    );
    for hash in [
        &intent.before_current,
        &intent.before_index,
        &intent.input_receipt,
    ] {
        nonzero_hash(hash)?;
    }
    evidence_text(&intent.evidence)?;
    ensure!(
        (1..=MAX_TARGETS).contains(&(intent.targets.len() + intent.playlists.len())),
        "a removal batch removes between one and {MAX_TARGETS} rows"
    );
    ensure!(
        intent.targets.is_empty() || intent.playlists.is_empty(),
        "a removal batch removes tracks or playlists, not both"
    );
    let mut containers = BTreeSet::new();
    for playlist in &intent.playlists {
        ensure!(
            playlist.container != 0
                && containers.insert(playlist.container)
                && playlist.name.len() <= 4096,
            "removal playlists are distinct nonzero containers with bounded names"
        );
    }
    ensure!(
        intent.playlists.is_empty()
            || (intent.compensates.is_none() && intent.compensates_insert_batch.is_none()),
        "a compensating batch removes no playlist"
    );
    let mut tracks = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for target in &intent.targets {
        validate_target(target)?;
        ensure!(
            tracks.insert(target.track),
            "removal batch lists a track twice"
        );
        if let Some(path) = &target.path {
            ensure!(
                paths.insert(path.to_ascii_lowercase()),
                "removal batch lists a path twice"
            );
        }
    }
    ensure!(
        intent.compensates.is_none() || intent.compensates_insert_batch.is_none(),
        "a compensating batch compensates an Insert or an insert batch, not both"
    );
    if intent.compensates.is_some() {
        ensure!(
            intent.targets.len() == 1
                && intent.targets[0].path.is_none()
                && intent.targets[0].binding.is_none(),
            "a compensating batch removes exactly one locationless unowned row"
        );
    }
    if intent.compensates_insert_batch.is_some() {
        ensure!(
            intent
                .targets
                .iter()
                .all(|t| t.path.is_none() && t.binding.is_none()),
            "a compensating batch removes only locationless unowned rows"
        );
    }
    Ok(())
}

impl Journal {
    /// The ID of the Active coordination that owns the journal until it
    /// closes.
    pub fn route_owner(&self) -> Result<Option<i64>> {
        active_id(&self.connection)
    }

    pub(super) fn coordination_storage(&self) -> Result<()> {
        self.usable()?;
        Ok(())
    }

    pub(super) fn coordination_writer(&self) -> Result<()> {
        self.coordination_storage()?;
        ensure!(
            self._read_only_copy.is_none(),
            "read-only journal cannot write coordination storage"
        );
        Ok(())
    }

    /// The Active coordination, if any.
    pub fn active_coordination(&self) -> Result<Option<CoordinationRecord>> {
        self.usable()?;
        active_id(&self.connection)?
            .map(|id| load_coordination(&self.connection, CoordinationId(id)))
            .transpose()
    }

    pub fn coordination(&self, id: CoordinationId) -> Result<CoordinationRecord> {
        self.coordination_storage()?;
        load_coordination(&self.connection, id)
    }

    /// Start a coordination on an idle journal with no Active coordination.
    /// From here ordinary writes refuse until it completes or is cancelled,
    /// except for the process that holds its permit.
    pub fn begin_coordination(
        &mut self,
        kind: CoordinationKind,
        intent: &[u8],
        evidence: &Path,
        base_anchor: u32,
    ) -> Result<CoordinationId> {
        self.coordination_writer()?;
        ensure!(
            (2..=MAX_INTENT).contains(&intent.len()),
            "coordination intent exceeds bounds"
        );
        let evidence = evidence_text(evidence)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_quiescent(&tx)?;
        if let Some(active) = active_id(&tx)? {
            bail!(
                "coordination {active} is already Active; run the command that began it again to finish it first"
            );
        }
        tx.execute(
            "INSERT INTO coordinations(kind,state,phase,revision,intent,intent_sha,evidence,base_anchor) VALUES(?1,0,0,0,?2,?3,?4,?5)",
            params![kind as i64, intent, digest(intent), evidence, base_anchor],
        )?;
        let id = CoordinationId(tx.last_insert_rowid());
        bindings::database_bound(&tx, 4 * 1024 * 1024)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(id)
    }

    /// Record forward progress. The phase may stay or grow, never shrink, and
    /// `expected_revision` must be current. Returns the new revision.
    pub fn advance_coordination(
        &mut self,
        id: CoordinationId,
        expected_revision: u64,
        phase: u8,
    ) -> Result<u64> {
        self.coordination_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_coordination(&tx, id)?;
        ensure!(
            current.state == CoordinationState::Active && current.revision == expected_revision,
            "stale or closed coordination revision"
        );
        ensure!(
            phase >= current.phase,
            "coordination phase cannot move backwards"
        );
        ensure!(
            tx.execute(
                "UPDATE coordinations SET phase=?1,revision=revision+1 WHERE id=?2 AND state=0 AND revision=?3",
                params![phase, id.0, i64::try_from(expected_revision)?],
            )? == 1,
            "coordination revision CAS differs"
        );
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(expected_revision + 1)
    }

    /// Admit this process as the Active coordination's writer. Ordinary
    /// journal writes opened by the coordinator pass the ownership check until
    /// the returned permit drops. One permit exists at a time per process.
    pub fn enter_coordination(&self, id: CoordinationId) -> Result<CoordinationPermit> {
        self.coordination_storage()?;
        let mut slot = permit_slot();
        ensure!(slot.is_none(), "a coordination permit is already held");
        ensure!(
            active_id(&self.connection)? == Some(id.0),
            "coordination is not the Active coordination"
        );
        *slot = Some((self.nonce, id.0));
        Ok(CoordinationPermit(()))
    }

    /// Playlists the Verified removal batches of coordination `id` deleted.
    pub fn coordination_deleted_playlists(&self, id: CoordinationId) -> Result<usize> {
        self.usable()?;
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM removal_playlists p JOIN removal_batches b ON b.id=p.batch JOIN coordination_steps s ON s.batch=b.id WHERE s.coordination=?1 AND b.state=2",
            [id.as_i64()],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }
}

/// An owned target must be the active version of a binding whose identity it
/// repeats; the SQL trigger checks the active version again.
fn check_owned_target(c: &Connection, target: &RemovalTarget) -> Result<()> {
    let Some((binding, version)) = target.binding else {
        return Ok(());
    };
    let (track, path): (Vec<u8>, String) = c
        .query_row(
            "SELECT r.track,r.path FROM active_bindings a JOIN binding_records r ON r.id=a.binding_id WHERE a.binding_id=?1 AND a.version=?2",
            params![binding.as_i64(), version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("removal target binding is not the active version")?;
    ensure!(
        track == target.track.to_be_bytes()
            && target
                .path
                .as_ref()
                .is_none_or(|p| p.eq_ignore_ascii_case(&path)),
        "removal target differs from its binding"
    );
    Ok(())
}
