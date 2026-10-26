//! Each engine event as records. Events that report a step's start count the
//! items before it as done; the step's end, or the next phase, completes it.
use super::{Level, Progress, notice, outcome, phase, progress, report};
use crate::{
    engine::execution::delete::Event as Deletion,
    engine::{ApplyResult, CoverResult, Event, Observation, SyncOutcome, SyncResult},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, sync::Mutex};

/// What earlier events said, for the records of later ones.
#[derive(Default)]
struct Tally {
    /// Tracks fingerprinted, of how many, and their bytes.
    index: (usize, usize, u64),
    /// The single addition under way: its number, of how many, and source.
    addition: Option<(usize, usize, String)>,
    /// The removal batch under way: tracks removed before it, its tracks,
    /// and all tracks.
    removal: Option<(usize, usize, usize)>,
    insert: Option<Insert>,
    /// The cover under way, of how many.
    covers: Option<(usize, usize)>,
    /// The phase whose counts the source pass under way reports; a pass
    /// that reports none is `inspect_sources`.
    inspecting: Option<&'static str>,
}

/// Insert batches under way.
struct Insert {
    batch: usize,
    batches: usize,
    total: usize,
    bytes_total: u64,
    /// Tracks and bytes of every upload that finished.
    uploaded: usize,
    uploaded_bytes: u64,
}

static TALLY: Mutex<Tally> = Mutex::new(Tally {
    index: (0, 0, 0),
    addition: None,
    removal: None,
    insert: None,
    covers: None,
    inspecting: None,
});

fn tally() -> std::sync::MutexGuard<'static, Tally> {
    TALLY.lock().unwrap_or_else(|e| e.into_inner())
}

fn path(path: &Path) -> Value {
    path.to_string_lossy().into_owned().into()
}

fn id(track: u64) -> String {
    format!("{track:016x}")
}

/// A source pass's count, in `phase`; its per-file reports follow it there.
fn sources_counted(phase: &'static str, done: usize, total: usize, line: String) -> Result<()> {
    tally().inspecting = Some(phase);
    progress(Progress::new(phase, "files", done, Some(total)).message(line))
}

fn inspecting() -> &'static str {
    tally().inspecting.unwrap_or("inspect_sources")
}

/// An upload's bytes: part of the insert batches under way, or of the single
/// addition.
fn upload(source: Option<&str>, track: u64, sent: u64, file_bytes: u64) -> Result<()> {
    let mut tally = tally();
    let mut current = json!({
        "track_id": id(track),
        "bytes_done": sent,
        "bytes_total": file_bytes,
    });
    let update = if let Some(insert) = tally.insert.as_mut() {
        current["batch"] = insert.batch.into();
        current["batches"] = insert.batches.into();
        let update = Progress::new("insert", "tracks", insert.uploaded, Some(insert.total))
            .bytes(insert.uploaded_bytes + sent, Some(insert.bytes_total));
        if sent == file_bytes {
            insert.uploaded += 1;
            insert.uploaded_bytes += file_bytes;
        }
        update
    } else {
        let (number, total) = tally
            .addition
            .as_ref()
            .map_or((1, None), |(number, total, _)| (*number, Some(*total)));
        if let Some((_, _, path)) = &tally.addition {
            current["path"] = path.as_str().into();
        }
        Progress::new("add", "tracks", number - 1, total)
    };
    if let Some(source) = source {
        current["path"] = source.into();
    }
    let name = current["path"].as_str().unwrap_or("audio").to_owned();
    drop(tally);
    progress(
        update
            .current(current)
            .message(format!("Uploading {name:?}: {sent}/{file_bytes} bytes")),
    )
}

fn apply_json(result: &ApplyResult) -> Value {
    let (result_name, additions, metadata_updates) = match *result {
        ApplyResult::Preflight { additions } => ("preflight", additions, 0),
        ApplyResult::Unchanged => ("unchanged", 0, 0),
        ApplyResult::Completed { additions } => ("completed", additions, 0),
        ApplyResult::PreflightSourceChanges {
            additions,
            metadata_updates,
        } => ("preflight", additions, metadata_updates),
    };
    json!({
        "result": result_name,
        "additions": additions,
        "metadata_updates": metadata_updates,
        "message": crate::cli::progress::applied(result),
    })
}

fn cover_json(result: &CoverResult) -> Value {
    let mut value = match *result {
        CoverResult::Unchanged => json!({"result": "unchanged"}),
        CoverResult::Planned {
            track_id,
            cache_id,
            bytes,
        } => json!({
            "result": "planned",
            "track_id": id(track_id),
            "cache_id": cache_id,
            "bytes": bytes,
        }),
        CoverResult::Verified => json!({"result": "verified"}),
    };
    value["message"] = crate::cli::progress::covered(result).into();
    value
}

/// A finished sync as its `sync` report.
pub(crate) fn sync_json(result: &SyncResult) -> Value {
    json!({
        "outcome": match result.outcome {
            SyncOutcome::Blocked => "blocked",
            SyncOutcome::Preflight => "dry_run",
            SyncOutcome::Completed => "completed",
        },
        "run": path(&result.run),
        "plan": path(&result.plan),
        "journal": path(&result.journal),
        "managed_covers_verified": result.managed_covers_verified,
        "existing_unmanaged_covers_unverified": result.existing_unmanaged_covers_unverified,
        "metadata_updates_verified": result.metadata_updates_verified,
        "covers_left_out": result.covers_left_out,
        "skipped": result.skipped,
        "skipped_count": result.skipped.len(),
        "message": crate::cli::progress::finished(result).join("\n"),
    })
}
