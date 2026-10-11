//! Exact read-only native row ancestry; no filesystem discovery or permit.
use super::{Binding, BindingRef, Identity, Journal, source_attribution};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::PathBuf;
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn managed_state(journal: &Journal, identity: &Identity) -> Result<PathBuf> {
    let path = &journal.directory_path;
    ensure!(
        path.file_name().and_then(|s| s.to_str()) == Some("journal"),
        "journal has no recognized managed state layout"
    );
    let library = path.parent().context("managed library path absent")?;
    let device = library.parent().context("managed device path absent")?;
    let devices = device.parent().context("managed devices path absent")?;
    ensure!(
        library.file_name().and_then(|s| s.to_str())
            == Some(hex(&identity.library_digest()).as_str())
            && device.file_name().and_then(|s| s.to_str())
                == Some(hex(&identity.device_digest()).as_str())
            && devices.file_name().and_then(|s| s.to_str()) == Some("devices"),
        "managed journal namespace identity differs"
    );
    let state = devices.parent().context("managed state absent")?;
    Ok(state.to_path_buf())
}
pub(crate) struct RowOrigin {
    reference: BindingRef,
    initial: super::Binding,
    attempt: super::Attempt,
}
impl RowOrigin {
    pub(crate) fn input_digest(&self) -> [u8; 32] {
        self.attempt.prepared.input_receipt_sha256
    }
    pub(crate) fn reference(&self) -> BindingRef {
        self.reference
    }
}
pub(crate) fn origin(
    journal: &Journal,
    binding: &Binding,
    identity: &Identity,
) -> Result<RowOrigin> {
    // Active bindings are keyed by encoded audio; only the binding's own key
    // can hold it.
    let active = journal
        .active_binding_for(&binding.content.source.audio.sha256)?
        .filter(|a| {
            a.binding.content == binding.content
                && a.binding.receipt_sha256 == binding.receipt_sha256
                && a.binding.media_anchor == binding.media_anchor
        })
        .context("active binding/version no longer matches planner binding")?;
    let reference = active.reference;
    let (initial, attempt, _) =
        source_attribution::native_origin(journal, reference, identity, true)?;
    Ok(RowOrigin {
        reference,
        initial,
        attempt,
    })
}
pub(crate) fn prior_digests(
    journal: &Journal,
    identity: &Identity,
    origin: &RowOrigin,
    plan_bytes: &[u8],
) -> Result<Vec<[u8; 32]>> {
    let initial = &origin.initial;
    let attempt = &origin.attempt;
    let value: Value = serde_json::from_slice(plan_bytes)?;
    let rows = value["sources"]
        .as_array()
        .context("historical plan sources absent")?;
    ensure!(rows.len() <= 10000, "historical plan row bound exceeded");
    let mut prior = Vec::new();
    for row in rows.iter().filter(|r| r["decision"] == "add") {
        if row["track_id"] == format!("{:016x}", initial.content.track_pid) {
            break;
        }
        ensure!(prior.len() < 32, "historical Add prefix bound exceeded");
        let pid = u64::from_str_radix(
            row["track_id"].as_str().context("prior target id absent")?,
            16,
        )?;
        let path = row["path"].as_str().context("prior target path absent")?;
        let mut q=journal.connection.prepare("SELECT r.id FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id AND v.version=1 JOIN attempts a ON a.id=r.first_attempt WHERE a.kind=1 AND a.state=2 AND r.track=?1 LIMIT 2")?;
        let ids = q
            .query_map([pid.to_be_bytes()], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            ids.len() == 1,
            "historical prior Add insertion is missing/ambiguous"
        );
        let mut matches = Vec::new();
        for id in ids {
            let (b, a, _) = source_attribution::native_origin(
                journal,
                BindingRef {
                    id: super::BindingId(id),
                    version: 1,
                },
                identity,
                false,
            )?;
            if b.content.path == path
                && a.prepared.current_media_anchor < attempt.prepared.current_media_anchor
            {
                matches.push(a);
            }
        }
        ensure!(
            matches.len() == 1,
            "historical prior Add insertion is missing/ambiguous"
        );
        prior.push(matches[0].prepared.input_receipt_sha256);
    }
    Ok(prior)
}
