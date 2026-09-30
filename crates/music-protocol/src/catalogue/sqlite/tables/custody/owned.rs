//! Owned creation custody for a prospective preparation. Snapshot facts alone
//! cannot construct this guard or acquire authority. No receiver qualification.
use super::*;

/// Retains the exact creation descriptors and original bounded SQL control.
/// No Clone, serde, output-path constructor, or Snapshot-to-owner conversion.
pub struct OwnedMaterialized {
    snapshot: Snapshot,
    output: CreatedOutput,
    budget: Budget,
}
impl OwnedMaterialized {
    pub fn ensure_original_control(
        &self,
        deadline: std::time::Instant,
        cancelled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        self.budget.ensure_original_control(deadline, cancelled)
    }
    pub fn recheck(&self) -> Result<()> {
        final_check(&self.snapshot, &self.output, &self.budget)
    }
    /// Only borrowed facts under the original owners. A returned Snapshot is
    /// still data; this owner cannot be reconstructed from it. Always perform
    /// the terminal custody checks, including after a visitor refusal.
    pub fn with_view<R>(
        &self,
        visitor: impl for<'a> FnOnce(Materialized<'a>) -> Result<R>,
    ) -> Result<R> {
        self.recheck()?;
        let result = visitor(Materialized {
            snapshot: &self.snapshot,
            output: &self.output,
            budget: &self.budget,
        });
        combine(result, self.recheck())
    }
}

/// The only construction path performs the real bounded private-copy SQL
/// materialization and moves its creation-held guards without reopening them.
pub fn materialize_owned_controlled(
    input: Capture<'_>,
    out: &Path,
    mut budget: Budget,
) -> Result<OwnedMaterialized> {
    let (snapshot, output) = acquire(input, out, &mut budget, |_| Ok(()), |_, _, _| Ok(()))?;
    let owned = OwnedMaterialized {
        snapshot,
        output,
        budget,
    };
    owned.recheck()?;
    Ok(owned)
}
