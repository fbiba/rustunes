//! Sessions. A session is one signed native operation that removes tracks and
//! then inserts others, as Apple's host sends a sync's removals and additions.
//! It is journaled as an insert batch and a removal batch paired with it
//! (`with_insert_batch`): both hold the session's operation, before state,
//! input receipt and evidence, and each keeps its own targets, so the
//! removal half retires bindings and the insert half binds new ones exactly as
//! batches of their own do. The halves change state together, in one
//! transaction each, the removal half first: Prepared, Started with the same
//! ready receipt and signature, then Verified with the insert half Verified,
//! Partial or Compensated, or both Cancelled or NotApplied. A Partial insert
//! half is compensated afterwards like any Partial insert batch. Triggers
//! hold the pairing and the order; `validate_exclusion` checks the states.
use super::coordination::{self, BatchId, RemovalIntent};
use super::insertion_batch::{
    self, InsertBatchId, InsertBatchState, InsertIntent, StartedInsertBatch,
};
use super::*;

/// The two halves of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionIds {
    pub insert: InsertBatchId,
    pub removal: BatchId,
}

impl Journal {
    /// Prepare both halves in one transaction on an idle journal. They must
    /// share the session's operation, anchor, before state, input receipt and
    /// evidence; the removal half removes tracks only.
    pub fn prepare_session(
        &mut self,
        insert: &InsertIntent,
        removal: &RemovalIntent,
    ) -> Result<SessionIds> {
        self.insert_batch_writer()?;
        self.coordination_writer()?;
        ensure!(
            insert.operation == removal.operation
                && insert.base_anchor == removal.base_anchor
                && insert.before_current == removal.before_current
                && insert.before_index == removal.before_index
                && insert.input_receipt == removal.input_receipt
                && insert.evidence == removal.evidence
                && insert.coordination.is_none(),
            "a session's halves share one operation, before state and evidence"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let insert = insertion_batch::prepare_insert(&tx, insert)?;
        let removal = coordination::prepare_removal(&tx, removal, Some(insert))?;
        insertion_batch::validate_exclusion(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(SessionIds { insert, removal })
    }

    /// Durable Prepared -> Started of both halves before the signed operation
    /// is sent; the insert half's permit carries the session's operation.
    pub fn start_session(
        &mut self,
        ids: SessionIds,
        ready_receipt: [u8; 32],
        signature: [u8; 21],
    ) -> Result<StartedInsertBatch> {
        self.insert_batch_writer()?;
        self.coordination_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let started = insertion_batch::start_insert(&tx, ids.insert, ready_receipt, signature)?;
        coordination::start_removal(&tx, ids.removal, ready_receipt, signature)?;
        insertion_batch::validate_exclusion(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(started)
    }

    /// A Prepared session that never started may be abandoned.
    pub fn cancel_session(&mut self, ids: SessionIds) -> Result<()> {
        self.insert_batch_writer()?;
        self.coordination_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        coordination::cancel_removal(&tx, ids.removal)?;
        insertion_batch::cancel_insert(&tx, ids.insert)?;
        insertion_batch::validate_exclusion(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }

    /// A sent session a fresh observation proved unapplied: nothing removed,
    /// nothing bound.
    pub fn mark_session_not_applied(
        &mut self,
        ids: SessionIds,
        proof_receipt: [u8; 32],
    ) -> Result<()> {
        self.insert_batch_writer()?;
        self.coordination_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        coordination::removal_not_applied(&tx, ids.removal, proof_receipt)?;
        insertion_batch::insert_not_applied(&tx, ids.insert, proof_receipt)?;
        insertion_batch::validate_exclusion(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }

    /// Settle a Started session from one fresh observation: the removal half
    /// Verified (every owned target retired), then the insert half from
    /// `bound` and `removed` as `settle_insert_batch` does. One transaction.
    pub fn settle_session(
        &mut self,
        ids: SessionIds,
        removal_receipt: [u8; 32],
        insert_receipt: [u8; 32],
        verified_anchor: u32,
        bound: &[(usize, CompletionEvidence)],
        removed: &[usize],
    ) -> Result<InsertBatchState> {
        self.insert_batch_writer()?;
        self.coordination_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        coordination::settle_removal(&tx, ids.removal, removal_receipt, verified_anchor)?;
        let state = insertion_batch::settle_insert(
            &tx,
            ids.insert,
            insert_receipt,
            verified_anchor,
            bound,
            removed,
        )?;
        validate_schema(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(state)
    }

    /// The removal half paired with insert batch `insert`, if it heads a
    /// session.
    pub fn session_removal(&self, insert: InsertBatchId) -> Result<Option<BatchRecord>> {
        self.coordination_storage()?;
        let id: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM removal_batches WHERE with_insert_batch=?1",
                [insert.as_i64()],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.removal_batch(BatchId(id))).transpose()
    }
}

#[cfg(test)]
mod tests;
