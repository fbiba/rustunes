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

pub(crate) fn event(event: &Event<'_>) -> Result<()> {
    let line = || -> Result<String> { Ok(crate::cli::progress::line(event)?.unwrap_or_default()) };
    use Level::{Info, Warning};
    match event {
        Event::VerificationStarting | Event::ReadingDeviceSnapshot => {
            phase("capture_device", Some(line()?))
        }
        Event::ReadingSavedSnapshot => phase("read_snapshot", Some(line()?)),
        Event::ComparingSources => phase("compare", Some(line()?)),
        Event::Deletion(event) => deletion(event),
        Event::ReadingCurrentState => phase("reconcile", Some(line()?)),
        Event::Observation(observation) => self::observation(observation),
        Event::Device(crate::engine::execution::device::Event::DeviceDetected(transport)) => {
            let (connection, address) = match transport {
                crate::engine::execution::device::Transport::Usb => ("usb", None),
                crate::engine::execution::device::Transport::Wifi(address) => {
                    ("wifi", Some(address.to_string()))
                }
            };
            notice(
                "device_detected",
                Info,
                line()?,
                json!({"connection": connection, "address": address}),
            )
        }
        Event::Device(crate::engine::execution::device::Event::PairingRecordAvailable) => {
            notice("pairing_record_available", Info, line()?, json!({}))
        }
        Event::RunEvidence(run) => {
            notice("run_directory", Info, line()?, json!({"path": path(run)}))
        }
        Event::InspectingSources { done, total } => {
            sources_counted("inspect_sources", *done, *total, line()?)
        }
        Event::PlanSaved(plan) => notice("plan_saved", Info, line()?, json!({"path": path(plan)})),
        Event::PlanReady(result) => report("plan", &crate::cli::plan_report(result)),
        Event::SyncBlocked(blocked) => {
            outcome("blocked");
            notice(
                "sync_blocked",
                Warning,
                line()?,
                json!({"holds": blocked.holds, "hint": blocked.hint}),
            )
        }
        Event::SyncFinished(result) => {
            tally().insert = None;
            outcome(match result.outcome {
                SyncOutcome::Blocked => "blocked",
                SyncOutcome::Preflight => "dry_run",
                SyncOutcome::Completed => "completed",
            });
            report("sync", &sync_json(result))
        }
        Event::CheckingCover { number, total } => {
            tally().covers = Some((*number, *total));
            progress(Progress::new("covers", "covers", number - 1, Some(*total)).message(line()?))
        }
        Event::ApplyFinished(result) => {
            outcome(match result {
                ApplyResult::Preflight { .. } | ApplyResult::PreflightSourceChanges { .. } => {
                    "dry_run"
                }
                ApplyResult::Unchanged => "nothing_to_do",
                ApplyResult::Completed { .. } => "completed",
            });
            report("apply", &apply_json(result))
        }
        Event::AdditionStarted {
            number,
            total,
            source,
        } => {
            tally().addition = Some((*number, *total, (*source).to_owned()));
            progress(
                Progress::new("add", "tracks", number - 1, Some(*total))
                    .current(json!({"path": source}))
                    .message(line()?),
            )
        }
        Event::AudioInsertionVerified => {
            let addition = tally().addition.clone();
            match addition {
                Some((number, total, source)) => progress(
                    Progress::new("add", "tracks", number, Some(total))
                        .current(json!({"path": source}))
                        .message(line()?),
                ),
                None => notice("insertion_verified", Info, line()?, json!({})),
            }
        }
        Event::SyncContinuing { remaining } => notice(
            "sync_continuing",
            Info,
            line()?,
            json!({"remaining": remaining}),
        ),
        Event::SyncResuming(reason) => {
            tally().insert = None;
            notice("sync_resuming", Warning, line()?, json!({"reason": reason}))
        }
        Event::CoverLeftOut(source) => {
            notice("cover_left_out", Warning, line()?, json!({"path": source}))
        }
        Event::FlacRefused { source, reason } => notice(
            "flac_refused",
            Warning,
            line()?,
            json!({"path": source, "reason": reason}),
        ),
        Event::ConvertingFlacs { directory } => notice(
            "converting_flacs",
            Info,
            line()?,
            json!({"directory": path(directory)}),
        ),
        Event::InsertBatchStarted {
            number,
            batches,
            tracks,
            inserted,
            total,
            bytes,
            removals,
        } => {
            let mut tally = tally();
            let insert = match tally.insert.take() {
                Some(insert) if insert.total == *total && insert.bytes_total == *bytes => insert,
                _ => Insert {
                    batch: 0,
                    batches: *batches,
                    total: *total,
                    bytes_total: *bytes,
                    uploaded: *inserted,
                    uploaded_bytes: 0,
                },
            };
            let insert = tally.insert.insert(Insert {
                batch: *number,
                ..insert
            });
            let update = Progress::new("insert", "tracks", insert.uploaded, Some(insert.total))
                .bytes(insert.uploaded_bytes, Some(insert.bytes_total))
                .current(json!({"batch": number, "batches": batches, "tracks": tracks, "removals": removals}))
                .message(match removals {
                    0 => format!(
                        "Insert batch {number}/{batches}: {tracks} new track(s); {inserted}/{total} on the phone"
                    ),
                    _ => format!(
                        "Insert batch {number}/{batches}: removing {removals} track(s), then {tracks} new track(s); {inserted}/{total} on the phone"
                    ),
                });
            drop(tally);
            progress(update)
        }
        Event::Uploading {
            source,
            track,
            sent,
            file_bytes,
        } => upload(*source, *track, *sent, *file_bytes),
        Event::InsertBatchVerified {
            inserted, total, ..
        } => {
            let tally = tally();
            let mut update = Progress::new("insert", "tracks", *inserted, Some(*total));
            if let Some(insert) = &tally.insert {
                update = update.bytes(insert.uploaded_bytes, Some(insert.bytes_total));
            }
            drop(tally);
            progress(update.message(line()?))
        }
        Event::UpdatesScheduled { tracks, sessions } => notice(
            "updates_scheduled",
            Info,
            line()?,
            json!({"tracks": tracks, "sessions": sessions}),
        ),
        Event::UpdatesRefused(reason) => notice(
            "updates_refused",
            Warning,
            line()?,
            json!({"reason": reason}),
        ),
        Event::MetadataBatchVerified { session, sessions } => progress(
            Progress::new("metadata", "sessions", *session, Some(*sessions)).message(line()?),
        ),
        Event::ReplacementPhase {
            coordination,
            phase,
        } => notice(
            "replacement_phase",
            Info,
            line()?,
            json!({"coordination": coordination, "phase": phase}),
        ),
        Event::RemovalBatchStarted {
            number,
            batches,
            tracks,
            removed,
            total,
        } => {
            tally().removal = Some((*removed, *tracks, *total));
            progress(
                Progress::new("remove", "tracks", *removed, Some(*total))
                    .current(json!({"batch": number, "batches": batches, "tracks": tracks}))
                    .message(format!(
                        "Removal batch {number}/{batches}: {tracks} track(s); {removed}/{total} removed"
                    )),
            )
        }
        Event::ReplacementRemoved { removed, total } => {
            progress(Progress::new("remove", "tracks", *removed, Some(*total)).message(line()?))
        }
        Event::RefreshingBeforeSession => {
            notice("refreshing_before_session", Info, line()?, json!({}))
        }
        Event::ArtworkRecentCountResets { plays, skips } => {
            if plays.is_empty() && skips.is_empty() {
                return Ok(());
            }
            notice(
                "recent_counter_resets",
                Warning,
                line()?,
                json!({"plays": plays, "skips": skips}),
            )
        }
        Event::ArtworkStarted { track_id } => notice(
            "artwork_started",
            Info,
            line()?,
            json!({"track_id": id(*track_id)}),
        ),
        Event::CoverFinished(result) => {
            let covers = tally().covers.take();
            match covers {
                // One of a sync's covers.
                Some((number, total)) => progress(
                    Progress::new("covers", "covers", number, Some(total))
                        .message(crate::cli::progress::covered(result)),
                ),
                None => {
                    outcome(match result {
                        CoverResult::Unchanged => "nothing_to_do",
                        CoverResult::Planned { .. } => "dry_run",
                        CoverResult::Verified => "completed",
                    });
                    report("artwork", &cover_json(result))
                }
            }
        }
        Event::JournalCarried {
            earlier,
            journal,
            carried,
            matched,
            unbound,
            gone,
            covers_proven,
            covers_unproven,
            unfinished,
        } => notice(
            "journal_carried",
            Info,
            line()?,
            json!({
                "earlier": path(earlier),
                "journal": path(journal),
                "carried": carried,
                "matched": matched,
                "unbound": unbound,
                "gone": gone,
                "covers_proven": covers_proven,
                "covers_unproven": covers_unproven,
                "unfinished": unfinished,
            }),
        ),
        Event::PullingTrack {
            number,
            total,
            track_id,
            title,
            artist,
            bytes,
            bytes_done,
            bytes_total,
        } => progress(
            Progress::new("pull_tracks", "tracks", number - 1, Some(*total))
                .bytes(*bytes_done, Some(*bytes_total))
                .current(json!({
                    "track_id": id(*track_id),
                    "title": title,
                    "artist": artist,
                    "bytes_total": bytes,
                }))
                .message(line()?),
        ),
        Event::TracksPulled { total, bytes_total } => progress(
            Progress::new("pull_tracks", "tracks", *total, Some(*total))
                .bytes(*bytes_total, Some(*bytes_total))
                .message("Finished copying phone tracks"),
        ),
    }
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

fn observation(event: &Observation<'_>) -> Result<()> {
    let line = crate::cli::progress::observation_line(event);
    use Level::{Info, Warning};
    match event {
        Observation::CapturingCatalogue => phase("capture_device", Some(line)),
        Observation::RevalidatingSources { done, total } => {
            sources_counted("revalidate_sources", *done, *total, line)
        }
        Observation::CatalogueCaptureRetained(directory) => notice(
            "catalogue_capture_retained",
            Info,
            line,
            json!({"path": path(directory)}),
        ),
        Observation::UntestedCatalogueVersion(version) => notice(
            "untested_catalogue_version",
            Warning,
            line,
            json!({"user_version": version, "tested": music_protocol::catalogue::sqlite::TESTED_USER_VERSIONS}),
        ),
        Observation::StateCaptureRetained(directory) => notice(
            "state_capture_retained",
            Info,
            line,
            json!({"path": path(directory)}),
        ),
        Observation::RecapturingBeforeAudioIndex
        | Observation::RecapturingAfterAudioIndex
        | Observation::RecapturingState
        | Observation::RecapturingAfterDroppedConnection => {
            notice("recapturing", Warning, line, json!({}))
        }
        Observation::FreshCatalogue { tracks, bytes } => {
            tally().index = (0, *tracks, 0);
            progress(
                Progress::new("index_audio", "tracks", 0, Some(*tracks))
                    .bytes(0, Some(*bytes))
                    .message(line),
            )
        }
        Observation::AudioFingerprinted {
            track,
            completed,
            total,
            bytes,
            other_codec,
            reused,
        } => {
            tally().index = (*completed, *total, *bytes);
            progress(
                Progress::new("index_audio", "tracks", *completed, Some(*total))
                    .bytes(*bytes, None)
                    .current(json!({
                        "track_id": id(*track),
                        "reused": reused,
                        "other_codec": other_codec,
                    }))
                    .message(line),
            )
        }
        Observation::CopyingTrackAudio {
            track,
            title,
            artist,
            read,
            file_bytes,
        } => {
            let (completed, total, bytes) = tally().index;
            progress(
                Progress::new("index_audio", "tracks", completed, Some(total))
                    .bytes(bytes + read, None)
                    .current(json!({
                        "track_id": id(*track),
                        "title": title,
                        "artist": artist,
                        "bytes_done": read,
                        "bytes_total": file_bytes,
                    }))
                    .message(line),
            )
        }
        Observation::AudioIndexRetained { tracks, bytes } => notice(
            "audio_index_retained",
            Info,
            line,
            json!({"tracks": tracks, "bytes": bytes}),
        ),
        Observation::VerifyingAudioBatch {
            number,
            total,
            tracks,
            bytes,
        } => progress(
            Progress::new("verify_audio", "batches", number - 1, Some(*total))
                .current(json!({"batch": number, "tracks": tracks, "bytes_total": bytes}))
                .message(line),
        ),
        Observation::AudioBatchChecked {
            number,
            total,
            matched,
            findings,
        } => progress(
            Progress::new("verify_audio", "batches", *number, Some(*total))
                .current(json!({"batch": number, "matched": matched, "findings": findings}))
                .message(line),
        ),
        Observation::VerifyingArtwork { number, total } => progress(
            Progress::new("verify_artwork", "covers", number - 1, Some(*total)).message(line),
        ),
        Observation::ReusedArtworkProofs { reused, total } => notice(
            "artwork_proofs_reused",
            Info,
            line,
            json!({"reused": reused, "total": total}),
        ),
        Observation::EmptyRendition { track, edge } => notice(
            "empty_rendition",
            Warning,
            line,
            json!({"track_id": id(*track), "edge": edge}),
        ),
        Observation::InspectingSource {
            number,
            total,
            path,
            file_bytes,
            bytes_done,
            bytes_total,
        } => progress(
            Progress::new(inspecting(), "files", number - 1, Some(*total))
                .bytes(*bytes_done, Some(*bytes_total))
                .current(json!({"path": path, "bytes_total": file_bytes}))
                .message(line),
        ),
        Observation::SourcesInspected { total, bytes_total } => {
            let phase = inspecting();
            tally().inspecting = None;
            progress(
                Progress::new(phase, "files", *total, Some(*total))
                    .bytes(*bytes_total, Some(*bytes_total))
                    .message(line),
            )
        }
        Observation::CacheUnavailable { cache, error } => notice(
            "cache_unavailable",
            Warning,
            line,
            json!({"cache": cache, "error": error}),
        ),
    }
}

/// A removal's events, from `delete`, `prune`, `clear` and `sync`.
pub(crate) fn deletion(event: &Deletion<'_>) -> Result<()> {
    let line = crate::cli::deletion::line(event);
    use Level::{Info, Warning};
    match event {
        Deletion::InspectingSources { done, total } => {
            sources_counted("inspect_sources", *done, *total, line.unwrap_or_default())
        }
        Deletion::Evidence(directory) => notice(
            "run_directory",
            Info,
            line.unwrap_or_default(),
            json!({"path": path(directory)}),
        ),
        Deletion::Selection {
            descriptor,
            path: selection,
        } => {
            report("removal_selection", descriptor)?;
            notice(
                "selection_saved",
                Info,
                format!("Selection: {}", selection.display()),
                json!({"path": path(selection)}),
            )
        }
        Deletion::Blocked => {
            outcome("blocked");
            notice(
                "removal_blocked",
                Warning,
                line.unwrap_or_default(),
                json!({}),
            )
        }
        Deletion::Held { tracks, sources } => notice(
            "removals_held",
            Warning,
            line.unwrap_or_default(),
            json!({"tracks": tracks, "sources": sources}),
        ),
        Deletion::DryRunComplete => {
            outcome("dry_run");
            notice(
                "dry_run_complete",
                Info,
                line.unwrap_or_default(),
                json!({}),
            )
        }
        Deletion::Cancelled => {
            outcome("cancelled");
            notice("cancelled", Info, line.unwrap_or_default(), json!({}))
        }
        Deletion::Step { number, total } => {
            let removal = tally().removal.take();
            let mut update = match removal {
                Some((removed, tracks, all)) => {
                    Progress::new("remove", "tracks", removed + tracks, Some(all))
                }
                None => Progress::new("remove", "batches", *number, Some(*total)),
            };
            update = update.current(json!({"batch": number, "batches": total}));
            progress(update.message(line.unwrap_or_default()))
        }
        Deletion::Settled { batch, state } => notice(
            "earlier_batch_settled",
            Info,
            line.unwrap_or_default(),
            json!({"batch": batch, "state": state}),
        ),
        Deletion::Completed {
            removed,
            deleted_playlists,
            left,
            evidence,
        } => {
            outcome(if *removed == 0 && *deleted_playlists == 0 && *left == 0 {
                "nothing_to_do"
            } else {
                "completed"
            });
            report(
                "removal",
                &json!({
                    "removed": removed,
                    "deleted_playlists": deleted_playlists,
                    "left_for_sync": left,
                    "evidence": evidence.map(path),
                    "message": crate::cli::deletion::completed_line(*removed, *deleted_playlists, *left, *evidence),
                }),
            )
        }
    }
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
