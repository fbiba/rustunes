//! Metadata-edit batches. One signed native metadata operation that
//! edits the metadata of several existing tracks. It is journaled Prepared ->
//! Started -> Verified, or Cancelled, or NotApplied after a fresh observation
//! proves a sent batch did not apply. The edit changes no audio file, binding
//! or reservation; the journal keeps only the intent, the tracks it names and
//! the evidence that frames the send.
//!
//! At most one native write of any kind is outstanding, enforced in Rust and by
//! triggers in every direction; see `insertion_batch` for the shared rules.
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
