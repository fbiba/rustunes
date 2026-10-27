//! Read-only presentation of a caller identity-bound local journal.
//!
//! This is stored evidence, not a device inventory or an execution permit. The
//! caller opens/checks identity and coordinates access; this module neither
//! creates a journal nor changes attempts, bindings or reservations.

use std::io::Write;

use anyhow::Result;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::engine::state::journal::{
    Attempt, AttemptKind, AttemptState, BatchCounts, BatchRecord, Content, CoordinationKind,
    CoordinationRecord, Journal, JournalHistorySummary,
};

const IDLE_GUIDANCE: &str = "No outstanding attempt. Fresh device evidence and a reviewed plan are still required before applying changes.";
const PREPARED_GUIDANCE: &str = "This attempt has no recorded Started transition. Revalidate before starting, or cancel it locally. Its reserved identities and paths must remain reserved.";
const COMPENSATION_GUIDANCE: &str = "This Started insertion has an outstanding removal batch that removes its file-less catalogue row. A Prepared batch is revalidated before sending, and a Started one is verified from fresh device evidence, never resent.";
const BATCH_GUIDANCE: &str = "A removal batch is outstanding. A Prepared batch can be cancelled locally; a Started one is verified or proven unapplied from fresh device evidence, never resent. Ordinary writes stay refused until it settles.";
const WRITE_BATCH_GUIDANCE: &str = "Run resume with the run's snapshot, this journal and a new trace directory: it cancels a Prepared batch locally and settles a Started one from fresh device evidence, never resending it. Ordinary writes stay refused until it settles.";
const STARTED_GUIDANCE: &str = "Outcome unknown; fresh reconciliation is required. Do not retry, reinsert, cancel, or allocate replacements. Run resume with the run's snapshot and run directory: it observes the phone and settles the attempt Verified, Compensated (its row without audio removed), NotApplied (the phone never took it) or, for a cover, Abandoned (its session ended before the image arrived; the next sync sends it again), never resending it.";

/// Exact 64-bit persistent IDs are hex strings, with signed decimal wire IDs
/// alongside them. Local attempt IDs are decimal strings, avoiding JSON number
/// precision loss. Hashes are complete lowercase hexadecimal values. The
/// text's continuing command ends with `connection` (`Engine::command_options`).
pub fn write(
    journal: &Journal,
    json_output: bool,
    out: &mut impl Write,
    connection: &str,
) -> Result<()> {
    let bindings = journal.active_verified_bindings()?;
    let outstanding = journal.outstanding_attempt()?;
    let coordination = journal.active_coordination()?;
    let batch = journal.outstanding_removal_batch()?;
    let batch_counts = journal.removal_batch_counts()?;
    let insert_batch = journal.outstanding_insert_batch()?;
    let metadata_batch = journal.outstanding_metadata_batch()?;
    let history = journal.history_summary()?;
    let pids = journal.reserved_pids()?;
    let caches = journal.reserved_cache_ids()?;
    let paths = journal.reserved_paths()?;
    let compensating = batch.as_ref().is_some_and(|b| b.compensates.is_some());
    let owner = journal.route_owner()?;
    let owner_guidance = owner.map(|id| {
        let next = if coordination.as_ref().is_some_and(|c| {
            crate::engine::execution::replace::continuation(c.kind, &c.intent, "").is_some()
        }) {
            "continue it with the command above"
        } else {
            "run the command that started it again to continue it"
        };
        format!(
            "coordination {id} owns the journal until it completes; ordinary writes stay refused. Each run starts from fresh device evidence: {next}."
        )
    });
    let guidance = match &outstanding {
        Some(a) if a.state == AttemptState::Prepared => PREPARED_GUIDANCE,
        Some(a) if a.state == AttemptState::ReconciliationRequired && compensating => {
            COMPENSATION_GUIDANCE
        }
        Some(a) if a.state == AttemptState::ReconciliationRequired => STARTED_GUIDANCE,
        _ if batch.is_some() => BATCH_GUIDANCE,
        _ if insert_batch.is_some() || metadata_batch.is_some() => WRITE_BATCH_GUIDANCE,
        _ => owner_guidance.as_deref().unwrap_or(IDLE_GUIDANCE),
    };
    if json_output {
        // A local record: status has neither current device evidence nor a
        // send permit.
        let mut report = json!({
            "schema":"rustunes.journal",
            "scope":"local_journal_evidence","device_state_checked":false,
            "execution_authorized":false,"phone_deletion_authorized":false,
            "binding_scope":"active_verified_only","active_verified_binding_count":bindings.len(),
            "verified_binding_count":bindings.len(),
            "bindings":bindings.iter().map(|binding|json!({"active":true,"verified_media_anchor":binding.media_anchor,
                "verification_receipt_sha256":hex(&binding.receipt_sha256),"verification_receipt_kind":binding.receipt_kind.as_str(),"content":content_json(&binding.content)})).collect::<Vec<_>>(),
            "outstanding":outstanding.as_ref().map(attempt_json),
            "history":history_json(&history),
            "reservations":{"include_historical_and_cancelled":true,"include_removal_targets":true,"reusable":false,
                "persistent_id_count":pids.len(),"persistent_ids":pids.iter().map(|id|format!("{id:016x}")).collect::<Vec<_>>(),
                "artwork_cache_id_count":caches.len(),"artwork_cache_ids":caches,"path_count":paths.len(),"paths_casefolded":paths},
            "recovery_guidance":guidance
        });
        report["coordination"] = coordination.as_ref().map_or(Value::Null, coordination_json);
        report["removal_batches"] = removal_batches_json(batch.as_ref(), &batch_counts);
        report["insert_batch"] = insert_batch.as_ref().map_or(Value::Null, |b| {
            write_batch_json(
                b.id.as_i64(),
                b.state.as_str(),
                b.targets.len(),
                b.base_anchor,
                &b.operation,
                &b.evidence,
            )
        });
        report["metadata_batch"] = metadata_batch.as_ref().map_or(Value::Null, |b| {
            write_batch_json(
                b.id.as_i64(),
                b.state.as_str(),
                b.tracks.len(),
                b.base_anchor,
                &b.operation,
                &b.evidence,
            )
        });
        serde_json::to_writer_pretty(&mut *out, &report)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(out, "Journal: {} active verified bindings", bindings.len())?;
    if let Some(c) = &coordination {
        writeln!(
            out,
            "Coordination {}: {} · phase {} · revision {} · evidence {}",
            c.id.as_i64(),
            coordination_kind(c.kind),
            c.phase,
            c.revision,
            terminal(&c.evidence.to_string_lossy())
        )?;
        if let Some(arguments) =
            crate::engine::execution::replace::continuation(c.kind, &c.intent, connection)
        {
            writeln!(out, "  Continue it with: rustunes {}", terminal(&arguments))?;
        }
    }
    if let Some(b) = &batch {
        let rows = match b.playlists.len() {
            0 => format!("{} targets", b.targets.len()),
            n => format!("{n} playlists"),
        };
        writeln!(
            out,
            "Removal batch {}: {} · {rows} · base anchor {}",
            b.id.as_i64(),
            b.state.as_str(),
            b.base_anchor
        )?;
    }
    writeln!(
        out,
        "Removal batches: {} verified · {} not applied",
        batch_counts.verified, batch_counts.not_applied
    )?;
    if let Some(b) = &insert_batch {
        writeln!(
            out,
            "Outstanding: insert batch {} · {} · {} tracks · base anchor {} · evidence {}",
            b.id.as_i64(),
            b.state.as_str(),
            b.targets.len(),
            b.base_anchor,
            terminal(&b.evidence.to_string_lossy())
        )?;
        for t in &b.targets {
            writeln!(
                out,
                "  Track: {:016x} · {}",
                t.target.content.track_pid,
                terminal(&t.target.content.path)
            )?;
        }
    }
    if let Some(b) = &metadata_batch {
        writeln!(
            out,
            "Outstanding: metadata batch {} · {} · {} tracks · base anchor {} · evidence {}",
            b.id.as_i64(),
            b.state.as_str(),
            b.tracks.len(),
            b.base_anchor,
            terminal(&b.evidence.to_string_lossy())
        )?;
    }
    match &outstanding {
        Some(a) => write_attempt(out, a)?,
        None if insert_batch.is_some() || metadata_batch.is_some() => {}
        None => writeln!(out, "Outstanding: none")?,
    }
    writeln!(out, "Recovery: {guidance}")?;
    writeln!(
        out,
        "Historical evidence: {} lifetimes · {} binding versions · {} retired bindings",
        history.lifetime_count, history.version_count, history.retired_binding_count,
    )?;
    writeln!(
        out,
        "Reservations: {} persistent IDs · {} artwork cache IDs · {} paths",
        pids.len(),
        caches.len(),
        paths.len()
    )?;
    writeln!(
        out,
        "Historical and cancelled reservations are retained; none are reusable."
    )?;
    writeln!(
        out,
        "Use --json for complete binding and reservation details."
    )?;
    writeln!(
        out,
        "\nLocal evidence only; this report performs no device check or state change."
    )?;
    Ok(())
}
fn write_attempt(out: &mut impl Write, a: &Attempt) -> Result<()> {
    writeln!(
        out,
        "Outstanding: attempt {} · {} · {}",
        a.id.as_i64(),
        kind(a.prepared.kind),
        state_human(a.state)
    )?;
    writeln!(
        out,
        "  Media revision: {} → {}",
        a.prepared.current_media_anchor,
        u64::from(a.prepared.current_media_anchor) + 1
    )?;
    write_content(out, &a.prepared.content)?;
    writeln!(
        out,
        "  Operation: {} bytes · SHA256 {}",
        a.prepared.operation.len(),
        hex(&Sha256::digest(&a.prepared.operation))
    )?;
    for (label, id) in [
        ("New album", a.prepared.new_album_pid),
        ("New artist", a.prepared.new_artist_pid),
    ] {
        if let Some(id) = id {
            writeln!(out, "  {label}: {id:016x}")?;
        }
    }
    Ok(())
}
fn coordination_kind(kind: CoordinationKind) -> &'static str {
    match kind {
        CoordinationKind::Clear => "whole Clear",
        CoordinationKind::ReplaceLibrary => "library reset and re-add",
        CoordinationKind::ReplaceTrack => "track replacement",
    }
}
fn coordination_json(c: &CoordinationRecord) -> Value {
    json!({"coordination_id":c.id.as_i64().to_string(),"kind":match c.kind {
            CoordinationKind::Clear => "clear",
            CoordinationKind::ReplaceLibrary => "replace_library",
            CoordinationKind::ReplaceTrack => "replace_track",
        },"phase":c.phase,"revision":c.revision.to_string(),
        "base_media_anchor":c.base_anchor,"intent_sha256":hex(&c.intent_sha256),
        "evidence_directory":c.evidence,
        "continue_arguments":crate::engine::execution::replace::continuation(c.kind, &c.intent, "")})
}
fn removal_batches_json(outstanding: Option<&BatchRecord>, counts: &BatchCounts) -> Value {
    json!({"outstanding":outstanding.map(|b|json!({"batch_id":b.id.as_i64().to_string(),
            "state":b.state.as_str(),"target_count":b.targets.len(),
            "playlist_count":b.playlists.len(),
            "compensates_attempt_id":b.compensates.map(|a|a.as_i64().to_string()),
            "coordination_id":b.coordination.map(|c|c.as_i64().to_string()),
            "base_media_anchor":b.base_anchor,"operation_sha256":hex(&Sha256::digest(&b.operation)),
            "evidence_directory":b.evidence})),
        "prepared_count":counts.prepared,"started_count":counts.started,
        "verified_count":counts.verified,"cancelled_count":counts.cancelled,
        "not_applied_count":counts.not_applied})
}
fn write_batch_json(
    id: i64,
    state: &str,
    tracks: usize,
    base_anchor: u32,
    operation: &[u8],
    evidence: &std::path::Path,
) -> Value {
    json!({"batch_id":id.to_string(),"state":state,"track_count":tracks,
        "base_media_anchor":base_anchor,"operation_sha256":hex(&Sha256::digest(operation)),
        "evidence_directory":evidence})
}
fn history_json(h: &JournalHistorySummary) -> Value {
    json!({"available":true,"scope":"historical_evidence_not_active_ownership","lifetime_count":h.lifetime_count,"version_count":h.version_count,"retired_binding_count":h.retired_binding_count})
}

fn attempt_json(attempt: &Attempt) -> Value {
    let p = &attempt.prepared;
    json!({ "attempt_id":attempt.id.as_i64().to_string(), "kind":kind(p.kind), "state":state(attempt.state),
        "started_recorded":attempt.state == AttemptState::ReconciliationRequired,
        "fresh_reconciliation_required":attempt.state == AttemptState::ReconciliationRequired,
        "current_media_anchor":p.current_media_anchor, "prepared_media_revision":u64::from(p.current_media_anchor)+1,
        "new_album_id":p.new_album_pid.map(|id|format!("{id:016x}")), "new_artist_id":p.new_artist_pid.map(|id|format!("{id:016x}")),
        "operation_bytes":p.operation.len(), "operation_sha256":hex(&Sha256::digest(&p.operation)),
        "input_receipt_sha256":hex(&p.input_receipt_sha256), "content":content_json(&p.content) })
}
fn content_json(content: &Content) -> Value {
    let source = &content.source;
    if let Some(a) = source.audio.mpeg() {
        return json!({"track_id":format!("{:016x}",content.track_pid),"wire_track_id":(content.track_pid as i64).to_string(),
            "afc_path":content.path,"collection_sha256":content.collection.map(|v|hex(&v)),
            "transferred_audio":{"file_bytes":source.file_bytes,"full_file_sha256":hex(&source.full_file_sha256),
                "encoded_mpeg1_layer3":{"sha256":hex(&source.audio.sha256),"frame_bytes_sha256":hex(&a.frame_bytes_sha256),
                    "sample_rate":a.sample_rate,"channels":a.channels,"frame_count":a.frame_count,
                    "audio_offset":a.audio_offset,"audio_bytes":a.audio_bytes,"trailing_bytes":a.trailing_bytes}},
            "artwork":content.artwork.as_ref().map(|art|json!({"cache_id":art.cache_id,"bytes":art.bytes,"sha256":hex(&art.sha256)}))});
    }
    if let Some(e) = source.audio.encoded() {
        let mut audio = crate::engine::state::audio_index::codec_json(
            &crate::engine::state::audio_index::OtherCodec::Encoded(e.clone()),
        );
        audio["sha256"] = json!(hex(&source.audio.sha256));
        return json!({"track_id":format!("{:016x}",content.track_pid),"wire_track_id":(content.track_pid as i64).to_string(),
            "afc_path":content.path,"collection_sha256":content.collection.map(|v|hex(&v)),
            "transferred_audio":{"file_bytes":source.file_bytes,"full_file_sha256":hex(&source.full_file_sha256),
                "encoded_audio":audio},
            "artwork":content.artwork.as_ref().map(|art|json!({"cache_id":art.cache_id,"bytes":art.bytes,"sha256":hex(&art.sha256)}))});
    }
    let audio = source
        .audio
        .alac()
        .expect("decoded Content has an exhaustive codec");
    json!({ "track_id":format!("{:016x}",content.track_pid), "wire_track_id":(content.track_pid as i64).to_string(),
        "afc_path":content.path, "collection_sha256":content.collection.map(|value|hex(&value)),
        "transferred_audio":{"file_bytes":source.file_bytes, "full_file_sha256":hex(&source.full_file_sha256),
            "encoded_alac":{"sha256":hex(&audio.sha256), "alac_config_sha256":hex(&audio.alac_config_sha256),
            "sample_sizes_sha256":hex(&audio.sample_sizes_sha256), "media_sha256":hex(&audio.media_sha256),
            "timescale":audio.timescale, "duration_ticks":audio.duration_ticks, "media_bytes":audio.media_bytes}},
        "artwork":content.artwork.as_ref().map(|art|json!({"cache_id":art.cache_id,"bytes":art.bytes,"sha256":hex(&art.sha256)})) })
}
fn write_content(out: &mut impl Write, content: &Content) -> Result<()> {
    writeln!(
        out,
        "  Track: {:016x} · wire {}",
        content.track_pid, content.track_pid as i64
    )?;
    writeln!(out, "  Path: {}", terminal(&content.path))?;
    writeln!(
        out,
        "  Transferred audio: {} bytes · SHA256 {}",
        content.source.file_bytes,
        hex(&content.source.full_file_sha256)
    )?;
    writeln!(
        out,
        "  Encoded {}: {}",
        content.source.format().label(),
        hex(&content.source.audio.sha256)
    )?;
    match &content.artwork {
        Some(art) => writeln!(
            out,
            "  Artwork: cache {} · {} bytes · SHA256 {}",
            art.cache_id,
            art.bytes,
            hex(&art.sha256)
        )?,
        None => writeln!(out, "  Artwork: no verified journal binding")?,
    }
    Ok(())
}
fn kind(value: AttemptKind) -> &'static str {
    match value {
        AttemptKind::Insert => "insert",
        AttemptKind::Artwork => "artwork",
    }
}
fn state(value: AttemptState) -> &'static str {
    match value {
        AttemptState::Prepared => "prepared",
        AttemptState::ReconciliationRequired => "started_requires_reconciliation",
        AttemptState::Verified => "verified",
        AttemptState::Cancelled => "cancelled",
        AttemptState::Compensated => "compensated",
        AttemptState::NotApplied => "not_applied",
        AttemptState::Abandoned => "abandoned",
    }
}
fn state_human(value: AttemptState) -> &'static str {
    match value {
        AttemptState::Prepared => "prepared",
        AttemptState::ReconciliationRequired => "started; reconciliation required",
        AttemptState::Verified => "verified",
        AttemptState::Cancelled => "cancelled",
        AttemptState::Compensated => "compensated; its catalogue row was removed again",
        AttemptState::NotApplied => "not applied; the phone never took it",
        AttemptState::Abandoned => {
            "abandoned; the cover never arrived and the next sync or push of its source sends it again"
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn terminal(text: &str) -> String {
    text.chars().flat_map(|c| {
        if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}') {
            c.escape_default().collect::<Vec<_>>()
        } else { vec![c] }
    }).collect()
}

#[cfg(test)]
mod tests;
