//! Metadata-edit batches. One signed native metadata operation that
//! edits the metadata of several existing tracks. It is journaled Prepared ->
//! Started -> Verified, or Cancelled, or NotApplied after a fresh observation
//! proves a sent batch did not apply. The edit changes no audio file, binding
//! or reservation; the journal keeps only the intent, the tracks it names and
//! the evidence that frames the send.
//!
//! At most one native write of any kind is outstanding, enforced in Rust and by
//! triggers in every direction; see `insertion_batch` for the shared rules.
use super::coordination::{self, CoordinationId, Work};
use super::*;

pub(super) const OUTSTANDING_BATCH: &str = "an outstanding metadata batch requires settlement";
/// Most tracks one metadata batch may edit.
const MAX_TRACKS: usize = 1000;

pub(super) fn outstanding_id(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row(
        "SELECT id FROM metadata_batches WHERE state IN(0,1)",
        [],
        |r| r.get(0),
    )
    .optional()?)
}

/// Fresh observations bound into the Prepared row. The operation is the exact
/// signed metadata operation, so its timestamp is fixed at preparation.
#[derive(Clone)]
pub struct MetadataIntent {
    /// The Active coordination this batch is the next step of, if any.
    pub coordination: Option<CoordinationId>,
    pub operation: Vec<u8>,
    pub base_anchor: u32,
    pub before_current: [u8; 32],
    pub before_index: [u8; 32],
    pub input_receipt: [u8; 32],
    /// Absolute run directory holding the saved Before evidence.
    pub evidence: PathBuf,
    /// The tracks the operation edits, in operation order.
    pub tracks: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MetadataBatchId(pub(super) i64);
impl MetadataBatchId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataBatchState {
    Prepared,
    Started,
    Verified,
    /// Never sent.
    Cancelled,
    /// Sent, then proven unapplied by a fresh observation.
    NotApplied,
}
impl MetadataBatchState {
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
            _ => bail!("invalid metadata batch state"),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataBatchRecord {
    pub id: MetadataBatchId,
    pub coordination: Option<CoordinationId>,
    pub state: MetadataBatchState,
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
    pub tracks: Vec<u64>,
}
/// Returned only after the durable Started commit. Not Clone: one send. A lost
/// value requires fresh reconciliation, never another permit.
pub struct StartedMetadataBatch {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedMetadataBatch {
    pub fn operation(&self) -> &[u8] {
        &self.operation
    }
    pub fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}

fn validate_intent(intent: &MetadataIntent) -> Result<()> {
    ensure!(
        !intent.operation.is_empty() && intent.operation.len() <= MAX_OPERATION,
        "metadata operation exceeds bounds"
    );
    ensure!(
        intent.base_anchor < u32::MAX,
        "metadata base anchor exhausted"
    );
    for hash in [
        &intent.before_current,
        &intent.before_index,
        &intent.input_receipt,
    ] {
        nonzero_hash(hash)?;
    }
    coordination::evidence_text(&intent.evidence)?;
    ensure!(
        !intent.tracks.is_empty() && intent.tracks.len() <= MAX_TRACKS,
        "a metadata batch edits between one and {MAX_TRACKS} tracks"
    );
    let mut seen = BTreeSet::new();
    for track in &intent.tracks {
        ensure!(*track != 0, "invalid metadata batch track identity");
        ensure!(seen.insert(*track), "metadata batch lists a track twice");
    }
    Ok(())
}

/// Cross-table facts of one batch; its own cells were checked by `load_batch`.
fn validate_batch(c: &Connection, record: &MetadataBatchRecord) -> Result<()> {
    ensure!(
        record.state == MetadataBatchState::Cancelled || !record.tracks.is_empty(),
        "a sent or settled metadata batch has no targets"
    );
    let step: Option<i64> = c
        .query_row(
            "SELECT coordination FROM coordination_steps WHERE metadata_batch=?1",
            [record.id.0],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        step == record.coordination.map(|id| id.as_i64()),
        "metadata batch coordination differs from its step"
    );
    Ok(())
}

impl Journal {
    fn metadata_batch_storage(&self) -> Result<()> {
        self.usable()?;
        Ok(())
    }

    fn metadata_batch_writer(&self) -> Result<()> {
        self.metadata_batch_storage()?;
        ensure!(
            self._read_only_copy.is_none(),
            "read-only journal cannot write metadata batch storage"
        );
        Ok(())
    }

    /// A Prepared batch that never started may be abandoned. Started batches
    /// have no cancel: they settle Verified or NotApplied from fresh evidence.
    pub fn cancel_metadata_batch(&mut self, id: MetadataBatchId) -> Result<()> {
        self.metadata_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE metadata_batches SET state=3 WHERE id=?1 AND state=0",
                [id.0]
            )? == 1,
            "only a Prepared metadata batch can be cancelled"
        );
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }

    /// Record that a sent batch was proven unapplied by a fresh observation.
    /// The tracks stay as they were.
    pub fn mark_metadata_batch_not_applied(
        &mut self,
        id: MetadataBatchId,
        proof_receipt: [u8; 32],
    ) -> Result<()> {
        self.metadata_batch_writer()?;
        nonzero_hash(&proof_receipt)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE metadata_batches SET state=4,result_receipt=?1 WHERE id=?2 AND state=1",
                params![proof_receipt, id.0],
            )? == 1,
            "only a Started metadata batch can be marked not applied"
        );
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }
}
