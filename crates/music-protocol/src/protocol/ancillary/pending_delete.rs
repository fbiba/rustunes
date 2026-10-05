//! Refusal-only ancillary profile for checked pending-delete operation data.
//! No conversion to ordinary MetadataPolicy and no session sender is exposed.
use super::*;
use crate::catalogue::mutation::pending_delete::PendingDelete;

/// Separate from installed-track metadata and pending-audio delivery. Every
/// own asset request, including audio, artwork, Genius and a zero mask, fails.
/// Successful validation yields only the foreign IDs whose unavailability the
/// caller may report.
pub struct PendingDeletePolicy<'a> {
    policy: AssetPolicy<'a, 0>,
}
impl EligibleSnapshot {
    pub fn pending_delete(
        &self,
        catalog: &Catalog,
        identity: &LibraryIdentity,
        deletion: &PendingDelete,
    ) -> Result<PendingDeletePolicy<'_>> {
        self.ensure_current(catalog, identity)?;
        deletion.ensure_current(catalog)?;
        ensure!(
            self.occupied.contains(&deletion.target_sync_id())
                && !self.tracks.contains_key(&deletion.target_sync_id()),
            "pending delete target is absent or unexpectedly installed"
        );
        Ok(PendingDeletePolicy {
            policy: AssetPolicy {
                snapshot: self,
                own: vec![deletion.target_asset_id()],
                own_request_profile: OwnRequestProfile::SinglePart,
                declared: Vec::new(),
            },
        })
    }
}
impl PendingDeletePolicy<'_> {
    pub fn own_asset_id(&self) -> &str {
        self.policy.own_asset_id()
    }
    pub fn ensure_session(&self, library_id: &str, anchor: u32) -> Result<()> {
        self.policy.ensure_session(library_id, anchor)
    }
    pub fn validate_manifest(&self, message: &Value, session: u32) -> Result<Vec<String>> {
        self.policy.validate_manifest(message, session)
    }
    /// The session's own-asset refusal wait. Not exported: callers cannot turn
    /// a pending-row policy into an installed-track metadata policy.
    pub(crate) fn refusal(&self) -> &AssetPolicy<'_, 0> {
        &self.policy
    }
}

#[cfg(test)]
pub(crate) mod tests;
