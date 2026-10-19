//! One signed native metadata-only session that edits several tracks' fields,
//! one `update_track` per track, journaled as a metadata batch.
//!
//! The edit is planned from a fresh complete state (`planning::metadata_edit`):
//! only fields and groupings the observed receiver rules cover are sent, and
//! a request outside them is refused before anything is prepared. Settlement
//! is by observation: all 45 tables must equal the modelled edit
//! (`verification::metadata_edit_sql`) and every audio file must be unchanged.
//! A session that provably did not apply becomes NotApplied; anything else
//! stays Started for `settle`. Nothing is ever resent.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use music_protocol::catalogue::metadata_edit::{self, Envelope, TrackUpdate};

use crate::{
    engine::execution::device,
    engine::execution::native_insert::{Control, create_trace, durable_write, sync_trace},
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::recovery::retained::playback::{Window, between_runs},
    engine::state::journal::{
        CoordinationId, Identity, Journal, MetadataBatchId, MetadataBatchRecord,
        MetadataBatchState, MetadataIntent, StartedMetadataBatch,
    },
    engine::state::snapshot::{self, Snapshot},
    engine::verification::audio_capture,
    engine::verification::sql_tables::reader::{self, Budget},
    engine::{
        execution::removal_batch::{self, Before, Sent, hex, next_directory, sha},
        planning::metadata_edit::{self as planner, EditRequest},
        recovery::stale_staging,
        verification::metadata_edit_sql,
    },
};
use music_protocol::catalogue::sqlite::Track;
use music_protocol::protocol::session::StartedOperation;
use music_protocol::protocol::trace::private_write;

impl StartedOperation for StartedMetadataBatch {
    fn operation(&self) -> &[u8] {
        StartedMetadataBatch::operation(self)
    }
    fn signature(&self) -> &[u8; 21] {
        StartedMetadataBatch::signature(self)
    }
}

/// The planned edit: what will be sent, for review before any write.
pub(crate) struct Planned {
    pub(crate) updates: Vec<TrackUpdate>,
    pub(crate) operation: Vec<u8>,
}

/// Plan `requests` against one exact catalogue state (`tables` and `catalog`
/// from the same capture). Requests that change nothing are dropped; an empty
/// plan sends nothing.
pub(crate) fn plan(
    tables: &planner::Tables,
    catalog: &music_protocol::catalogue::sqlite::Catalog,
    requests: &[EditRequest],
) -> Result<Planned> {
    let mut random = std::fs::File::open("/dev/urandom")?;
    let mut fresh = || {
        let mut bytes = [0; 8];
        // A read failure yields 0, which the planner never uses as an id.
        random
            .read_exact(&mut bytes)
            .map(|()| u64::from_le_bytes(bytes) & (i64::MAX as u64))
            .unwrap_or(0)
    };
    let updates = planner::plan(tables, requests, &mut fresh)?;
    let operation = if updates.is_empty() {
        Vec::new()
    } else {
        metadata_edit::encode(
            &Envelope {
                database_pid: catalog.database_pid(),
                primary_container_pid: catalog.primary_container_pid(),
                revision: catalog
                    .persisted_media_anchor()
                    .checked_add(1)
                    .context("media anchor exhausted")?,
                timestamp: UNIX_EPOCH
                    + Duration::from_secs(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()),
            },
            &updates,
        )?
    };
    Ok(Planned { updates, operation })
}

/// What a batch ended as.
pub(crate) struct Settled {
    pub(crate) batch: MetadataBatchId,
    pub(crate) state: MetadataBatchState,
    pub(crate) evidence: PathBuf,
    pub(crate) updated: usize,
}

/// Prepare, send and settle one planned edit from the exact `before` state.
/// Errors before Started leave nothing behind but a Prepared batch that is
/// cancelled here; after Started the batch is settled by observation or left
/// Started for `settle`.
pub(crate) async fn run(
    journal: &mut Journal,
    identity: &Identity,
    before: &Before,
    planned: &Planned,
    coordination: Option<CoordinationId>,
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    control.check()?;
    ensure!(!planned.updates.is_empty(), "the edit changes nothing");
    ensure!(
        journal.outstanding_metadata_batch()?.is_none(),
        "an earlier metadata batch is unfinished; run resume first"
    );
    // The plan is checked against this exact state again before preparing.
    planner::check_conformant(&before.sql.tables, &planned.updates)?;
    let current = &before.current;
    let base_anchor = before.anchor();
    let (envelope, decoded) = metadata_edit::decode(&planned.operation)?;
    ensure!(
        decoded == planned.updates
            && envelope.revision
                == base_anchor
                    .checked_add(1)
                    .context("media anchor exhausted")?
            && envelope.database_pid == current.catalog.database_pid()
            && envelope.primary_container_pid == current.catalog.primary_container_pid(),
        "the edit operation was not planned against this state"
    );
    let evidence = &before.directory;
    let tracks: Vec<u64> = planned.updates.iter().map(|u| u.sync_id).collect();
    let input = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.metadata-batch-input",
        "base_anchor":base_anchor,
        "operation_sha256":hex(&sha(&planned.operation)),
        "before_index_sha256":hex(&before.snapshot.index_receipt_sha256),
        "before_current_sha256":hex(&current.receipt_sha256),
        "host_name":host_name,
        "tracks":tracks.iter().map(|t| format!("{t:016x}")).collect::<Vec<_>>(),
    }))?;
    durable_write(evidence, "input.json", &input)?;
    durable_write(evidence, "operation.plist", &planned.operation)?;
    let input_sha = sha(&input);
    let id = journal.prepare_metadata_batch(&MetadataIntent {
        coordination,
        operation: planned.operation.clone(),
        base_anchor,
        before_current: current.receipt_sha256,
        before_index: before.snapshot.index_receipt_sha256,
        input_receipt: input_sha,
        evidence: evidence.clone(),
        tracks: tracks.clone(),
    })?;
    let send = evidence.join("send");
    let wire = match removal_batch::send_metadata_only(
        before,
        &planned.operation,
        removal_batch::Requests::Refuse(tracks[0]),
        input_sha,
        host_name,
        &send,
        control,
        |ready, signature| journal.start_metadata_batch(id, ready, signature),
    )
    .await
    {
        Sent::Unsent(error) => {
            journal.cancel_metadata_batch(id)?;
            return Err(error);
        }
        Sent::Started(wire) => wire.and_then(|(ready, terminal, operation_sha)| {
            durable_write(
                &send,
                "wire.json",
                &serde_json::to_vec_pretty(&json!({
                    "schema":"rustunes.metadata-batch-wire","batch":id.as_i64(),
                    "ready_sha256":hex(&ready),"terminal_sha256":hex(&terminal),
                    "operation_sha256":hex(&operation_sha),
                }))?,
            )?;
            sync_trace(&send)
        }),
    };
    if let Err(error) = &wire {
        let _ = durable_write(
            &send,
            "error.txt",
            format!(
                "{error:#}\nBatch {} is Started; it is observed below.\n",
                id.as_i64()
            )
            .as_bytes(),
        );
    }
    let batch = journal.metadata_batch(id)?;
    match (
        settle(journal, identity, &batch, Window::SameRun, control).await,
        wire,
    ) {
        (Ok(settled), _) => Ok(settled),
        (Err(error), Ok(())) => Err(error.context(format!(
            "metadata batch {} is Started; run resume to observe it again",
            id.as_i64()
        ))),
        (Err(error), Err(wire)) => Err(error.context(format!(
            "metadata batch {} is Started after a transport error ({wire:#}); run resume to observe it again",
            id.as_i64()
        ))),
    }
}

/// Observe a Started batch and settle it: Verified when all 45 tables are the
/// modelled edit and every audio file is unchanged; NotApplied when the anchor
/// did not move, no target changed and nothing is staged but this batch's own
/// unfinished operation pair, which is removed first. Otherwise it stays
/// Started and the error says why; the full SQL delta is kept.
pub(crate) async fn settle(
    journal: &mut Journal,
    identity: &Identity,
    batch: &MetadataBatchRecord,
    window: Window,
    control: &Control,
) -> Result<Settled> {
    ensure!(
        batch.state == MetadataBatchState::Started,
        "metadata batch {} is not Started",
        batch.id.as_i64()
    );
    let evidence = &batch.evidence;
    let before = snapshot::open(&evidence.join("snapshot"))?;
    ensure!(
        before.index_receipt_sha256 == batch.before_index,
        "retained before snapshot differs from the journaled batch"
    );
    let run = next_directory(evidence, "observe")?;
    create_trace(&run)?;
    let before_sql = removal_batch::materialize(&before, &run.join("sql-before"), control)?;
    let after_dir = run.join("snapshot");
    audio_capture::capture_with_control(&after_dir, control, device::provider).await?;
    let after = snapshot::open(&after_dir)?;
    let after_sql = removal_batch::materialize(&after, &run.join("sql-after"), control)?;
    let budget =
        Budget::new_cancellable_until(control.deadline.into_std(), Arc::clone(&control.cancelled));
    if let Ok(delta) = reader::delta(&before_sql, &after_sql, &budget) {
        durable_write(&run, "sql-delta.json", &serde_json::to_vec(&delta)?)?;
    }
    files_unchanged(&before, &after)?;
    let anchor = after.catalog.persisted_media_anchor();
    let targets: BTreeSet<u64> = batch.tracks.iter().copied().collect();
    if anchor == batch.base_anchor {
        let kept = |s: &Snapshot| -> Vec<_> {
            s.catalog
                .tracks()
                .iter()
                .filter(|t| targets.contains(&t.sync_id))
                .cloned()
                .collect()
        };
        ensure!(
            targets_unchanged(&kept(&before), &kept(&after), window),
            "the anchor did not move but a target changed; batch {} stays Started",
            batch.id.as_i64()
        );
        let library = music_protocol::catalogue::identity::parse(&after.raw_preferences)?;
        let staged = removal_batch::staged_names(identity, &library, &run, control).await?;
        let staged_removed = removal_batch::remove_unfinished(
            identity,
            &library,
            &run,
            &staged,
            stale_staging::Unfinished {
                send: &batch.evidence.join("send"),
                operation: &batch.operation,
                signature: batch.signature,
            },
            batch.id.as_i64(),
            control,
        )
        .await?;
        let proof = serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.metadata-batch-not-applied","batch":batch.id.as_i64(),
            "anchor":anchor,"after_index_sha256":hex(&after.index_receipt_sha256),
            "targets_unchanged":true,"staging_empty":true,"staged_removed":staged_removed,
        }))?;
        durable_write(&run, "not-applied.json", &proof)?;
        journal.mark_metadata_batch_not_applied(batch.id, sha(&proof))?;
        return Ok(Settled {
            batch: batch.id,
            state: MetadataBatchState::NotApplied,
            evidence: run,
            updated: 0,
        });
    }
    let receipt = match window {
        Window::SameRun => metadata_edit_sql::verify(
            &before_sql.tables,
            &after_sql.tables,
            &batch.operation,
            batch.base_anchor,
        ),
        Window::LaterRun => metadata_edit_sql::verify_later(
            &before_sql.tables,
            &after_sql.tables,
            &batch.operation,
            batch.base_anchor,
        ),
    }
    .with_context(|| {
        format!(
            "the phone's catalogue is not the modelled edit; batch {} stays Started and {} keeps the delta",
            batch.id.as_i64(),
            run.display()
        )
    })?;
    let mut verification = json!({
        "schema":"rustunes.metadata-batch-verification","batch":batch.id.as_i64(),
        "base_anchor":batch.base_anchor,"verified_anchor":anchor,
        "before_index_sha256":hex(&batch.before_index),
        "after_index_sha256":hex(&after.index_receipt_sha256),
        "edit":receipt,"audio_files_unchanged":true,
    });
    removal_batch::allocation_changed(&mut verification, &before, &after);
    let bytes = serde_json::to_vec_pretty(&verification)?;
    durable_write(&run, "verification.json", &bytes)?;
    journal.settle_metadata_batch(batch.id, sha(&bytes), anchor)?;
    Ok(Settled {
        batch: batch.id,
        state: MetadataBatchState::Verified,
        evidence: run,
        updated: receipt.updated(),
    })
}

/// Whether the targets' catalogue rows are as before: exactly within the
/// batch's own run; for a later run, apart from plays and skips since
/// (`catalogue_listening`).
fn targets_unchanged(old: &[Track], new: &[Track], window: Window) -> bool {
    match window {
        Window::SameRun => old == new,
        Window::LaterRun => {
            old.len() == new.len()
                && old.iter().zip(new).all(|(was, now)| {
                    let mut played = now.clone();
                    played.playback = was.playback.clone();
                    played == *was
                        && between_runs::catalogue_listening(&was.playback, &now.playback).is_ok()
                })
        }
    }
}

/// An edit never touches audio: every indexed file is as before (its
/// allocation aside, `TrackAudio::same_file`).
fn files_unchanged(before: &Snapshot, after: &Snapshot) -> Result<()> {
    ensure!(
        before.index.same_files(&after.index),
        "audio files changed during a metadata edit"
    );
    Ok(())
}

/// The `metadata set` request: explicit field edits for tracks on the phone.
pub struct SetOptions<'a> {
    pub requests: Vec<EditRequest>,
    /// Plan against this saved snapshot instead of the phone; dry runs only.
    pub snapshot: Option<&'a Path>,
    pub journal: Option<&'a Path>,
    pub state: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
    /// How this command reached the phone, repeated in a `resume` it names
    /// (`Engine::command_options`).
    pub connection: &'a str,
}

pub enum SetOutcome {
    /// Every requested value is already on the phone.
    Unchanged,
    Preview,
    Cancelled,
    Applied {
        updated: usize,
    },
}

fn preview(
    planned: &Planned,
    run: &Path,
    show: impl FnOnce(&[TrackUpdate], &Path) -> Result<()>,
) -> Result<SetOutcome> {
    let path = run.join("edit.plist");
    if planned.updates.is_empty() {
        show(&[], run)?;
        return Ok(SetOutcome::Unchanged);
    }
    private_write(&path, &planned.operation)?;
    show(&planned.updates, &path)?;
    Ok(SetOutcome::Preview)
}

/// One track's source-driven Update: the track, the encoded audio its source
/// holds (the track must still hold it) and the source's projected tags.
pub(crate) struct SourceUpdate {
    pub(crate) track: u64,
    pub(crate) audio: UpdatedAudio,
    pub(crate) desired: crate::engine::planning::additive::ProjectedFields,
}

/// What the track must still hold on the phone when its update is sent.
pub(crate) enum UpdatedAudio {
    /// The source's encoded ALAC audio.
    Alac(music_protocol::media::fingerprint::EncodedAlac),
    /// The MP3 file this tool sent, by its SHA-256: the source shares its
    /// frames, while its tags (and so its file) differ.
    Mpeg([u8; 32]),
    /// The ALAC this tool converted from the source FLAC and sent, by its
    /// SHA-256.
    Converted([u8; 32]),
}

impl UpdatedAudio {
    fn held_by(
        &self,
        index: &crate::engine::state::audio_index::DeviceAudioIndex,
        track: u64,
    ) -> bool {
        match self {
            Self::Alac(audio) => index
                .tracks()
                .get(&track)
                .is_some_and(|a| a.fingerprint.audio == *audio),
            Self::Mpeg(file) => index
                .other_audio()
                .get(&track)
                .is_some_and(|a| a.full_file_sha256 == *file),
            Self::Converted(file) => index
                .tracks()
                .get(&track)
                .is_some_and(|a| a.fingerprint.full_file_sha256 == *file),
        }
    }
}

/// The plan's Update rows, each bound to its source again: the file must be
/// the one planned, and its tags are what the track's fields become.
pub(crate) fn source_updates(
    plan: &crate::engine::planning::additive::Plan,
) -> Result<Vec<SourceUpdate>> {
    let base = Path::new(&plan.selection.base_directory);
    let mut updates = Vec::new();
    for row in &plan.sources {
        let crate::engine::planning::additive::Decision::Update { track_id, .. } = &row.decision
        else {
            continue;
        };
        let path = base.join(&row.relative_path);
        if let Some(planned) = &row.mp3 {
            updates.push(mpeg_update(row, planned, &path, track_id)?);
            continue;
        }
        let fingerprint = music_protocol::media::fingerprint::read(&path)?;
        let desired = crate::engine::planning::additive::ProjectedFields::from_source(
            &music_protocol::media::metadata::read(&path)?,
        )?;
        let full: String = fingerprint
            .full_file_sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        ensure!(
            row.source_file_sha256.as_deref() == Some(full.as_str())
                && row.projected.as_ref() == Some(&desired),
            "{} changed after planning",
            row.relative_path
        );
        updates.push(SourceUpdate {
            track: u64::from_str_radix(track_id, 16)?,
            audio: UpdatedAudio::Alac(fingerprint.audio),
            desired,
        });
    }
    Ok(updates)
}

/// An owned MP3's Update (or a FLAC's whose ALAC this tool converted and
/// sent): the source read again must be the one planned, and the phone must
/// still hold the file this tool sent.
fn mpeg_update(
    row: &crate::engine::planning::additive::SourceDecision,
    planned: &music_protocol::media::mpeg::Source,
    path: &Path,
    track_id: &str,
) -> Result<SourceUpdate> {
    let changed = || format!("{} changed after planning", row.relative_path);
    let prepared = PreparedSource::read_expected(path, planned).with_context(changed)?;
    let desired = prepared.projected()?;
    ensure!(row.projected.as_ref() == Some(&desired), "{}", changed());
    let (converted, device) = match &row.matches[..] {
        [found]
            if matches!(
                found.match_basis,
                Some("owned_mpeg_frame_identity" | "flac_conversion")
            ) =>
        {
            (
                found.match_basis == Some("flac_conversion"),
                found
                    .device_file_sha256
                    .as_deref()
                    .context("the planned source names no phone file")?,
            )
        }
        _ => anyhow::bail!(
            "{} is neither an owned MP3 matched by its frames nor a FLAC converted to ALAC",
            row.relative_path
        ),
    };
    let mut file = [0; 32];
    ensure!(device.len() == 64, "invalid phone file digest");
    for (i, byte) in file.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&device[i * 2..i * 2 + 2], 16)?;
    }
    Ok(SourceUpdate {
        track: u64::from_str_radix(track_id, 16)?,
        audio: if converted {
            UpdatedAudio::Converted(file)
        } else {
            UpdatedAudio::Mpeg(file)
        },
        desired,
    })
}

/// At most this many sessions per sync; each moves at least one field.
const MAX_SESSIONS: usize = 256;

/// The edit requests that remain for `updates` against one capture: each
/// track must still hold its source's audio, and group names are spelled as
/// the phone already spells them.
fn source_requests(
    updates: &[SourceUpdate],
    snapshot: &Snapshot,
    cat: &planner::Catalogue,
) -> Result<Vec<EditRequest>> {
    let mut requests = Vec::new();
    for update in updates {
        let track = snapshot
            .catalog
            .tracks()
            .iter()
            .find(|t| t.sync_id == update.track)
            .with_context(|| format!("track {:016x} is no longer on the phone", update.track))?;
        ensure!(
            update.audio.held_by(&snapshot.index, update.track),
            "track {:016x} no longer holds its source's audio",
            update.track
        );
        if let Some(mut request) = crate::engine::planning::additive::update::request(
            &update.desired,
            &snapshot.catalog,
            track,
        )? {
            cat.respell(&mut request)?;
            requests.push(request);
        }
    }
    Ok(requests)
}

/// The sessions the planned updates need against a saved snapshot, or why
/// they cannot all be sent. Nothing is sent; `directory` receives the SQL.
pub(crate) fn schedule_saved(
    updates: &[SourceUpdate],
    snapshot: &Snapshot,
    directory: &Path,
    control: &Control,
) -> Result<Vec<Vec<u64>>> {
    let sql = removal_batch::materialize(snapshot, directory, control)?;
    let cat = planner::Catalogue::read(&sql.tables)?;
    let requests = source_requests(updates, snapshot, &cat)?;
    planner::schedule(&sql.tables, &requests, MAX_SESSIONS, &mut fresh_ids()?)
}

fn fresh_ids() -> Result<impl FnMut() -> u64> {
    let mut random = std::fs::File::open("/dev/urandom")?;
    Ok(move || {
        let mut bytes = [0; 8];
        // A read failure yields 0, which the planner never uses as an id.
        random
            .read_exact(&mut bytes)
            .map(|()| u64::from_le_bytes(bytes) & (i64::MAX as u64))
            .unwrap_or(0)
    })
}

/// Bring each track's catalogue fields to its source's tags in verified
/// metadata batches: as many sessions as the receiver's group rules need,
/// each planned from its own fresh capture under `directory/session-NNN`.
/// Before the first, the whole sequence is scheduled against that capture
/// (`planning::metadata_edit::schedule`), so a sequence that could not finish
/// sends nothing. Returns the number of tracks updated.
pub(crate) async fn update_all(
    journal_path: &Path,
    updates: &[SourceUpdate],
    coordination: Option<CoordinationId>,
    directory: &Path,
    host_name: &str,
    control: impl Fn() -> Control,
    mut progress: impl FnMut(usize, usize) -> Result<()>,
) -> Result<usize> {
    let mut fresh = fresh_ids()?;
    let mut tracks = 0;
    let mut planned_sessions = 0;
    for session in 1..=MAX_SESSIONS + 1 {
        let control = control();
        let (identity, before) = removal_batch::capture_fresh(
            &directory.join(format!("session-{session:03}")),
            &control,
        )
        .await?;
        let cat = planner::Catalogue::read(&before.sql.tables)?;
        let requests = source_requests(updates, &before.snapshot, &cat)?;
        if requests.is_empty() {
            return Ok(tracks);
        }
        ensure!(
            session <= MAX_SESSIONS,
            "metadata updates need more than {MAX_SESSIONS} sessions"
        );
        if session == 1 {
            planned_sessions =
                planner::schedule(&before.sql.tables, &requests, MAX_SESSIONS, &mut fresh)?.len();
            tracks = requests.len();
        }
        let (kept, waiting) = planner::pack(&cat, &requests, &mut fresh);
        ensure!(
            !kept.is_empty(),
            "{} metadata update(s) cannot be sent after {} session(s):\n{}",
            waiting.len(),
            session - 1,
            waiting
                .iter()
                .map(|(track, reason)| format!("  {track:016x}: {reason}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let planned = plan(&before.sql.tables, &before.current.catalog, &kept)?;
        let mut journal = Journal::open(journal_path, &identity)?;
        let settled = run(
            &mut journal,
            &identity,
            &before,
            &planned,
            coordination,
            host_name,
            &control,
        )
        .await?;
        ensure!(
            settled.state == MetadataBatchState::Verified,
            "the phone did not apply metadata batch {} ({}); evidence: {}",
            settled.batch.as_i64(),
            settled.state.as_str(),
            settled.evidence.display()
        );
        progress(session, planned_sessions.max(session))?;
    }
    unreachable!("the session loop returns or refuses")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_run_lets_an_unapplied_batchs_targets_have_been_played() {
        let mut old = music_protocol::test_support::catalogue::collection_catalog(13)
            .tracks()
            .to_vec();
        old[0].playback.play_count_user = 5;
        let mut played = old.clone();
        played[0].playback.play_count_user = 7;
        played[0].playback.play_count_recent += 2;
        played[0].playback.has_been_played = true;
        assert!(targets_unchanged(&old, &old, Window::SameRun));
        assert!(!targets_unchanged(&old, &played, Window::SameRun));
        assert!(targets_unchanged(&old, &played, Window::LaterRun));
        let mut rated = played.clone();
        rated[0].playback.user_rating = 80;
        let mut renamed = played.clone();
        renamed[0].metadata.title = "Renamed".into();
        let mut fewer = old.clone();
        fewer[0].playback.play_count_user = 4;
        for wrong in [rated, renamed, fewer, played[1..].to_vec()] {
            assert!(!targets_unchanged(&old, &wrong, Window::LaterRun));
        }
    }
}
