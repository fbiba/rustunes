//! Active bindings, the outstanding attempt and the journal's history counts.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindingRef {
    pub id: BindingId,
    pub version: u32,
}
pub struct ActiveBinding {
    pub reference: BindingRef,
    pub binding: Binding,
}

/// Counts of binding lifetimes and tombstones in the journal's history.
pub struct JournalHistorySummary {
    pub lifetime_count: u64,
    pub version_count: u64,
    pub retired_binding_count: u64,
}

impl Journal {
    pub fn history_summary(&self) -> Result<JournalHistorySummary> {
        self.ordinary_inspectable()?;
        let count = |sql| -> Result<u64> {
            Ok(u64::try_from(
                self.connection.query_row(sql, [], |r| r.get::<_, i64>(0))?,
            )?)
        };
        Ok(JournalHistorySummary {
            lifetime_count: count("SELECT count(*) FROM binding_records")?,
            version_count: count("SELECT count(*) FROM binding_versions")?,
            retired_binding_count: count("SELECT count(*) FROM binding_tombstones")?,
        })
    }
}
