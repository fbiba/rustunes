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

fn load_batch(c: &Connection, id: MetadataBatchId) -> Result<MetadataBatchRecord> {
    type Raw = (
        Option<i64>,
        i64,
        Vec<u8>,
        Vec<u8>,
        u32,
        [Vec<u8>; 3],
        String,
        (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>),
        Option<u32>,
    );
    let raw: Raw = c
        .query_row(
            "SELECT coordination,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence,ready_receipt,signature,result_receipt,verified_anchor FROM metadata_batches WHERE id=?1",
            [id.0],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    [r.get(5)?, r.get(6)?, r.get(7)?],
                    r.get(8)?,
                    (r.get(9)?, r.get(10)?, r.get(11)?),
                    r.get(12)?,
                ))
            },
        )
        .optional()?
        .context("metadata batch not found")?;
    ensure!(
        coordination::digest(&raw.2) == array::<32>(&raw.3)?,
        "metadata batch operation digest differs"
    );
    let mut tracks = Vec::new();
    {
        let mut query = c.prepare(
            "SELECT ordinal,track FROM metadata_targets WHERE batch=?1 ORDER BY ordinal LIMIT 1001",
        )?;
        let mut rows = query.query([id.0])?;
        while let Some(r) = rows.next()? {
            ensure!(tracks.len() < MAX_TRACKS, "metadata batch target bound");
            ensure!(
                usize::try_from(r.get::<_, i64>(0)?)? == tracks.len(),
                "metadata batch targets have a gap"
            );
            tracks.push(u64::from_be_bytes(array(&r.get::<_, Vec<u8>>(1)?)?));
        }
    }
    let [before_current, before_index, input_receipt] = raw.5;
    let (ready, signature, result) = raw.7;
    let record = MetadataBatchRecord {
        id,
        coordination: raw.0.map(CoordinationId),
        state: MetadataBatchState::decode(raw.1)?,
        operation: raw.2,
        base_anchor: raw.4,
        before_current: array(&before_current)?,
        before_index: array(&before_index)?,
        input_receipt: array(&input_receipt)?,
        evidence: PathBuf::from(raw.6),
        ready_receipt: ready.as_deref().map(array).transpose()?,
        signature: signature.as_deref().map(array).transpose()?,
        result_receipt: result.as_deref().map(array).transpose()?,
        verified_anchor: raw.8,
        tracks,
    };
    if record.state != MetadataBatchState::Cancelled || !record.tracks.is_empty() {
        validate_intent(&MetadataIntent {
            coordination: record.coordination,
            operation: record.operation.clone(),
            base_anchor: record.base_anchor,
            before_current: record.before_current,
            before_index: record.before_index,
            input_receipt: record.input_receipt,
            evidence: record.evidence.clone(),
            tracks: record.tracks.clone(),
        })?;
    }
    for receipt in [&record.ready_receipt, &record.result_receipt]
        .into_iter()
        .flatten()
    {
        nonzero_hash(receipt)?;
    }
    if let Some(signature) = &record.signature {
        ensure!(
            signature[0] == 1,
            "invalid metadata batch signature envelope"
        );
    }
    if let Some(anchor) = record.verified_anchor {
        ensure!(
            anchor > record.base_anchor,
            "verified metadata anchor does not cover its revision"
        );
    }
    Ok(record)
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

/// Every metadata batch of a journal, plus the facts that join them.
pub(super) fn validate_rows(c: &Connection) -> Result<()> {
    ensure!(
        c.query_row("SELECT count(*) FROM metadata_batches", [], |r| r
            .get::<_, i64>(0))?
            <= coordination::MAX_BATCHES,
        "metadata batch history bound"
    );
    let ids = c
        .prepare("SELECT id FROM metadata_batches ORDER BY id")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let record = load_batch(c, MetadataBatchId(id))?;
        validate_batch(c, &record)?;
    }
    ensure!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata_targets t LEFT JOIN metadata_batches b ON b.id=t.batch WHERE b.id IS NULL)",
            [],
            |r| r.get::<_, bool>(0)
        )?,
        "a metadata target names a missing batch"
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

    pub fn metadata_batch(&self, id: MetadataBatchId) -> Result<MetadataBatchRecord> {
        self.metadata_batch_storage()?;
        load_batch(&self.connection, id)
    }

    /// Every metadata batch, oldest first.
    #[cfg(test)]
    pub fn metadata_batches(&self) -> Result<Vec<MetadataBatchRecord>> {
        self.usable()?;
        let ids = self
            .connection
            .prepare("SELECT id FROM metadata_batches ORDER BY id")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| load_batch(&self.connection, MetadataBatchId(id)))
            .collect()
    }

    /// The single Prepared or Started batch, if any.
    pub fn outstanding_metadata_batch(&self) -> Result<Option<MetadataBatchRecord>> {
        self.usable()?;
        outstanding_id(&self.connection)?
            .map(|id| load_batch(&self.connection, MetadataBatchId(id)))
            .transpose()
    }

    /// Durable local intent only, with its tracks and (for a coordinated
    /// batch) the coordination's next step in one transaction. This grants no
    /// native permit.
    pub fn prepare_metadata_batch(&mut self, intent: &MetadataIntent) -> Result<MetadataBatchId> {
        self.metadata_batch_writer()?;
        validate_intent(intent)?;
        let evidence = coordination::evidence_text(&intent.evidence)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match intent.coordination {
            Some(id) => ensure!(
                coordination::active_id(&tx)? == Some(id.as_i64()),
                "metadata batch names a coordination that is not Active"
            ),
            None => coordination::ensure_no_route_owner(&tx)?,
        }
        coordination::ensure_quiescent(&tx)?;
        bindings::database_bound(&tx, 4 * 1024 * 1024)?;
        tx.execute(
            "INSERT INTO metadata_batches(coordination,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(?1,0,?2,?3,?4,?5,?6,?7,?8)",
            params![
                intent.coordination.map(|c| c.as_i64()),
                intent.operation,
                coordination::digest(&intent.operation),
                intent.base_anchor,
                intent.before_current,
                intent.before_index,
                intent.input_receipt,
                evidence
            ],
        )?;
        let id = MetadataBatchId(tx.last_insert_rowid());
        for (ordinal, track) in intent.tracks.iter().enumerate() {
            tx.execute(
                "INSERT INTO metadata_targets(batch,ordinal,track) VALUES(?1,?2,?3)",
                params![id.0, i64::try_from(ordinal)?, track.to_be_bytes()],
            )?;
        }
        if let Some(coordination) = intent.coordination {
            coordination::link_step(&tx, coordination, Work::Metadata(id))?;
        }
        bindings::database_bound(&tx, 4 * 1024 * 1024)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(id)
    }

    /// Durable Prepared -> Started before the signed operation is sent. There
    /// is no retry: a lost `StartedMetadataBatch` requires fresh reconciliation.
    pub fn start_metadata_batch(
        &mut self,
        id: MetadataBatchId,
        ready_receipt: [u8; 32],
        signature: [u8; 21],
    ) -> Result<StartedMetadataBatch> {
        self.metadata_batch_writer()?;
        nonzero_hash(&ready_receipt)?;
        ensure!(signature[0] == 1, "unsupported metadata signature envelope");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let batch = load_batch(&tx, id)?;
        ensure!(
            batch.state == MetadataBatchState::Prepared,
            "metadata batch already started or closed; reconcile instead of retrying"
        );
        ensure!(
            tx.execute(
                "UPDATE metadata_batches SET state=1,ready_receipt=?1,signature=?2 WHERE id=?3 AND state=0",
                params![ready_receipt, signature.as_slice(), id.0],
            )? == 1,
            "metadata batch start CAS differs"
        );
        bindings::database_bound(&tx, 4 * 1024 * 1024)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(StartedMetadataBatch {
            operation: batch.operation,
            signature,
        })
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

    /// Verify a Started batch from fresh evidence. This releases the journal
    /// for new writes; no binding changes, because the audio files did not.
    pub fn settle_metadata_batch(
        &mut self,
        id: MetadataBatchId,
        result_receipt: [u8; 32],
        verified_anchor: u32,
    ) -> Result<()> {
        self.metadata_batch_writer()?;
        nonzero_hash(&result_receipt)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let batch = load_batch(&tx, id)?;
        ensure!(
            batch.state == MetadataBatchState::Started,
            "metadata batch is not Started"
        );
        ensure!(
            verified_anchor > batch.base_anchor,
            "verified metadata edit does not cover its revision"
        );
        ensure!(
            tx.execute(
                "UPDATE metadata_batches SET state=2,result_receipt=?1,verified_anchor=?2 WHERE id=?3 AND state=1",
                params![result_receipt, verified_anchor, id.0],
            )? == 1,
            "metadata batch verification CAS differs"
        );
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }
}

#[cfg(test)]
mod tests;
