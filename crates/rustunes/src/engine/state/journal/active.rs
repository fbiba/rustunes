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

    /// The active binding of this audio identity, with its reference.
    #[cfg(test)]
    pub fn lookup_active(
        &self,
        audio: &impl music_protocol::media::fingerprint::EncodedIdentity,
    ) -> Result<Option<ActiveBinding>> {
        self.ordinary_inspectable()?;
        active(&self.connection, audio.encoded_sha256())
    }
    /// The active binding stored under one encoded-audio key, if any.
    pub(crate) fn active_binding_for(&self, audio: &[u8; 32]) -> Result<Option<ActiveBinding>> {
        self.ordinary_inspectable()?;
        active(&self.connection, audio)
    }
    /// An outstanding removal batch, insert batch or metadata batch is
    /// outstanding work too, but not an attempt: it refuses here
    /// with its own message.
    pub fn outstanding(&self) -> Result<Option<Attempt>> {
        self.ordinary_inspectable()?;
        coordination::ensure_no_outstanding_batch(&self.connection)?;
        self.outstanding_attempt()
    }
    /// The outstanding attempt alone, ignoring any outstanding batch. Status
    /// shows it beside them.
    pub(crate) fn outstanding_attempt(&self) -> Result<Option<Attempt>> {
        self.ordinary_inspectable()?;
        let id: Option<i64> = self
            .connection
            .query_row("SELECT id FROM attempts WHERE state IN(0,1)", [], |r| {
                r.get(0)
            })
            .optional()?;
        id.map(|id| load_attempt(&self.connection, AttemptId(id)))
            .transpose()
    }
}

fn active(c: &Connection, audio: &[u8; 32]) -> Result<Option<ActiveBinding>> {
    let reference: Option<(i64, u32)> = c
        .query_row(
            "SELECT binding_id,version FROM active_bindings WHERE audio=?1",
            [audio],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    reference
        .map(|(id, version)| {
            Ok(ActiveBinding {
                reference: BindingRef {
                    id: BindingId(id),
                    version,
                },
                binding: lookup(c, audio)?.context("active binding missing")?,
            })
        })
        .transpose()
}
