//! End-to-end additive synchronization using the same saved-plan/apply boundary.
use std::{
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{
    engine::execution::apply,
    engine::execution::cover,
    engine::execution::device,
    engine::planning::additive::{ArtworkDecision, Decision},
    engine::planning::saved_plan,
    engine::planning::source_collection::SelectionOptions,
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::journal::{CoordinationId, Identity, Journal},
    engine::state::layout,
    engine::state::snapshot,
    engine::verification::audio_capture,
    engine::{
        Engine, Event, Hold, PlanRequest, SkipStage, Skipped, SyncHolds, SyncOutcome, SyncResult,
        execution::{insert_batch, metadata_batch},
        planning::change::MAX_BATCH,
    },
};
use music_protocol::protocol::trace::private_write;

#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub source: &'a Path,
    pub state: &'a Path,
    pub snapshot: Option<&'a Path>,
    pub journal: Option<&'a Path>,
    pub selection: &'a SelectionOptions,
    pub host_name: &'a str,
    pub dry_run: bool,
    pub seconds: u64,
    pub add_only: bool,
    /// Operations per native session: each new track and each removed track
    /// is one. At 1 each track has a session of its own, and an MP3 only a
    /// batch can send goes in an insert batch of its own.
    pub batch_size: usize,
    /// `--flac-to-alac`: send each new FLAC as an ALAC converted from it.
    pub flac_to_alac: bool,
    /// A saved plan's additions and updates (`apply`): this run's plan must
    /// name exactly these before anything is sent. The fresh syncs a run
    /// continues with are not pinned.
    pub pinned: Option<&'a crate::engine::ChangePins>,
    /// The library directory phone tracks are matched to when the journal
    /// must be carried forward first (`pull`); `None` carries only the
    /// earlier journal's bindings.
    pub library: Option<&'a Path>,
    /// Tracks the removal before this sync left it, fewer than a batch
    /// (`DeletionOutcome::Completed::left`): the first insert batch removes
    /// them in its session, before its new tracks, as Apple's host sends a
    /// sync. Without an insert batch they go in removal batches of their own,
    /// first; when the plan matches one, they go first and the sync plans
    /// again without them.
    pub removals: &'a [u64],
}

/// How many times a sync plans again on its own after an insert batch was
/// cut short and undone, as when the phone stops answering mid-upload.
const CONTINUATIONS: usize = 3;

pub(crate) async fn run(options: Options<'_>, engine: &Engine) -> Result<SyncResult> {
    let mut result = Box::pin(run_with(options, None, engine)).await;
    let mut removed = 0;
    if let Some(first) = result.as_ref().err().and_then(removed_first) {
        removed += first.0;
        result = Box::pin(run_with(
            Options {
                snapshot: None,
                removals: &[],
                ..options
            },
            None,
            engine,
        ))
        .await;
    }
    // A continuation finds the removals done: they went with the first batch.
    let options = Options {
        removals: &[],
        ..options
    };
    for _ in 0..CONTINUATIONS {
        let Some(cut) = result.as_ref().err().and_then(cut_short) else {
            break;
        };
        removed += cut.1;
        engine.emit(Event::SyncResuming(&cut.0))?;
        result = Box::pin(run_with(
            Options {
                snapshot: None,
                pinned: None,
                ..options
            },
            None,
            engine,
        ))
        .await;
    }
    result.map(|mut result| {
        result.removed += removed;
        result
    })
}

/// Why an insert batch was cut short and undone, if that stopped the sync.
fn cut_short(error: &anyhow::Error) -> Option<&insert_batch::CutShort> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<insert_batch::CutShort>())
}

/// The removals left to the sync went first, alone (this many tracks),
/// because the plan matched one of their tracks or no insert batch takes
/// them; the sync plans again without them.
#[derive(Debug)]
struct RemovedFirst(usize);
impl std::fmt::Display for RemovedFirst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} removals left to this sync went first, alone",
            self.0
        )
    }
}
impl std::error::Error for RemovedFirst {}

fn removed_first(error: &anyhow::Error) -> Option<&RemovedFirst> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RemovedFirst>())
}

/// The same sync, with its insert batches linked as steps of an Active
/// `coordination` (a replacement); the caller holds its permit.
pub(crate) async fn run_with(
    options: Options<'_>,
    coordination: Option<CoordinationId>,
    engine: &Engine,
) -> Result<SyncResult> {
    engine.check()?;
    ensure!(
        (1..=1800).contains(&options.seconds),
        "capture deadline must be 1..1800 seconds"
    );
    ensure!(
        (1..=MAX_BATCH).contains(&options.batch_size),
        "batch size must be 1..{MAX_BATCH}"
    );
    ensure!(
        (!options.dry_run && coordination.is_none()) || options.removals.is_empty(),
        "only a sync that sends removes the tracks a removal left it"
    );
    // Invalid local selection must fail before expensive device work.
    crate::engine::planning::source_collection::select(options.source, options.selection)?;
    let run = layout::new_run(options.state)?;
    engine.emit(Event::RunEvidence(&run))?;
    let snapshot_path = match options.snapshot {
        Some(path) => std::fs::canonicalize(path).context("opening supplied snapshot")?,
        None => {
            let path = run.join("snapshot");
            engine.emit(Event::ReadingDeviceSnapshot)?;
            let control = engine.control(options.seconds);
            control.check()?;
            Box::pin(audio_capture::capture_with_control(
                &path,
                &control,
                device::provider,
            ))
            .await?;
            path
        }
    };
    let saved = snapshot::open(&snapshot_path)?;
    let journal = match options.journal {
        Some(path) => std::path::absolute(path)?,
        // A replacement carried its journal forward before it began.
        None if coordination.is_none() => {
            crate::engine::execution::pull::current_journal(
                options.state,
                &saved,
                options.library,
                options.selection.recursive,
                &run,
                &engine.control(options.seconds),
            )
            .await?
        }
        None => layout::journal_path(
            options.state,
            &saved.identity.device_identifier_sha256,
            &saved.identity.library_identity_sha256,
        )?,
    };
    if coordination.is_none() && journal.try_exists()? {
        let identity = Identity::for_saved_device(
            saved.identity.device_identifier_sha256,
            &music_protocol::catalogue::identity::parse(&saved.raw_preferences)?,
        )?;
        crate::engine::execution::replace::ensure_no_unfinished_attempt(
            &journal,
            &identity,
            options.state,
            &snapshot_path,
            &engine.command_options(),
        )?;
    }
    let plan = run.join("plan.json");
    let plan_options = options.selection.clone();
    let source = options.source.to_path_buf();
    let saved_path = snapshot_path.clone();
    let plan_path = plan.clone();
    let journal_option = journal.try_exists()?.then(|| journal.clone());
    let add_only = options.add_only;
    let flac_to_alac = options.flac_to_alac;
    let pinned = options.pinned.cloned();
    let left: Vec<String> = options
        .removals
        .iter()
        .map(|t| format!("{t:016x}"))
        .collect();
    let planning_engine = engine.clone();
    let ready = tokio::task::spawn_blocking(move || {
        let report = planning_engine.plan(PlanRequest {
            source: &source,
            snapshot: &saved_path,
            journal: journal_option.as_deref(),
            output: Some(&plan_path),
            selection: &plan_options,
            add_only,
            flac_to_alac,
        })?;
        planning_engine.emit(Event::PlanReady(&report))?;
        let skipped = Skipped::planned(&report.plan);
        // The plan was made with the removals' tracks on the phone; one it
        // matches must go first, and the sync plan again without it.
        let preserved = &report.plan.preserved_unmatched_device_tracks;
        if left.iter().any(|track| !preserved.contains(track)) {
            return Ok::<_, anyhow::Error>((None, Vec::new(), skipped, true));
        }
        if let Some(pinned) = &pinned {
            pinned.check_sync(&report.plan)?;
        }
        if report.ready() {
            return Ok((
                None,
                metadata_batch::source_updates(&report.plan)?,
                skipped,
                false,
            ));
        }
        Ok((Some(holds(&report.plan)), Vec::new(), skipped, false))
    })
    .await
    .context("planning task failed")??;
    let (held, updates, mut skipped, remove_first) = ready;
    let descriptor = serde_json::json!({"schema":"rustunes.run", "snapshot":snapshot_path,
        "journal":journal,"plan":plan,"dry_run":options.dry_run});
    private_write(
        &run.join("run.json"),
        &serde_json::to_vec_pretty(&descriptor)?,
    )?;
    File::open(&run)?.sync_all()?;
    if remove_first {
        let removed = Box::pin(remove_alone(&options, &saved, &journal, &run, engine)).await?;
        return Err(RemovedFirst(removed).into());
    }
    if let Some(holds) = held {
        // The removals go even so, as they would have before the sync.
        let removed = Box::pin(remove_alone(&options, &saved, &journal, &run, engine)).await?;
        let hint = hint(&holds, &options, coordination, &engine.command_options());
        engine.emit(Event::SyncBlocked(&SyncHolds { holds, hint }))?;
        let mut result = SyncResult::new(SyncOutcome::Blocked, &run, &plan, &journal, 0, 0);
        result.removed = removed;
        return Ok(result);
    }
    if !updates.is_empty() {
        // Schedule every session before any write: a sequence the receiver's
        // group rules cannot finish sends nothing.
        match metadata_batch::schedule_saved(
            &updates,
            &saved,
            &run.join("update-schedule"),
            &engine.control(options.seconds),
        ) {
            Ok(sessions) => engine.emit(Event::UpdatesScheduled {
                tracks: updates.len(),
                sessions: sessions.len(),
            })?,
            Err(error) => {
                let removed =
                    Box::pin(remove_alone(&options, &saved, &journal, &run, engine)).await?;
                engine.emit(Event::UpdatesRefused(&format!("{error:#}")))?;
                engine.emit(Event::SyncBlocked(&SyncHolds {
                    holds: Vec::new(),
                    hint: None,
                }))?;
                let mut result = SyncResult::new(SyncOutcome::Blocked, &run, &plan, &journal, 0, 0);
                result.removed = removed;
                return Ok(result);
            }
        }
    }
    if options.dry_run {
        // A supplied snapshot can be old; a live preflight makes this explicit.
        Box::pin(apply::run(
            apply::Options {
                plan: &plan,
                snapshot: &snapshot_path,
                journal: &journal,
                trace: &run.join("preflight"),
                host_name: options.host_name,
                dry_run: true,
            },
            engine,
        ))
        .await?;
        let mut result = SyncResult::new(SyncOutcome::Preflight, &run, &plan, &journal, 0, 0);
        result.skipped = skipped;
        engine.emit(Event::SyncFinished(&result))?;
        return Ok(result);
    }
    // Retain exact source expectations before the audio phase changes the journal.
    let checked = saved_plan::load_sync_expectations(&plan, &snapshot_path, &journal)?;
    // With a batch size of one, only MP3s are batched (one per batch).
    let mut batched = Some(insert_batch::members(&checked, options.batch_size)?)
        .filter(|b| !b.members.is_empty());
    let mut covers = Vec::new();
    let protected_albums: std::collections::BTreeSet<_> = saved
        .catalog
        .tracks()
        .iter()
        .map(|t| t.album_sync_id)
        .collect();
    let bound_tracks: std::collections::BTreeSet<_> = checked
        .journal
        .as_ref()
        .map(|j| j.active_verified_bindings())
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.content.track_pid)
        .collect();
    let albums_with_unmanaged_tracks: std::collections::BTreeSet<_> = saved
        .catalog
        .tracks()
        .iter()
        .filter(|t| !bound_tracks.contains(&t.sync_id))
        .map(|t| t.album_sync_id)
        .collect();
    let mut existing_covers_unverified = 0usize;
    for row in &checked.plan.sources {
        // Nothing of a skipped source is sent, its cover included.
        if matches!(row.decision, Decision::Skip { .. }) {
            continue;
        }
        if matches!(row.decision, Decision::Add { .. })
            && let Some(source) = checked
                .sources
                .mp3_tracks
                .iter()
                .find(|s| s.relative_path == row.relative_path)
        {
            // An insert batch sends it, with its cover.
            PreparedSource::read_expected(
                &checked.sources.base_directory.join(&source.relative_path),
                &source.source,
            )?
            .ensure_batch_insertion_allowed()?;
            continue;
        }
        if let Some(has_cover) = preserved_mpeg_keep(row, &checked.sources)? {
            existing_covers_unverified += usize::from(has_cover);
            continue;
        }
        if options.add_only
            && !matches!(row.decision, Decision::Add { .. })
            && !matches!(row.artwork, ArtworkDecision::PendingOwnedCover { .. })
        {
            if matches!(
                row.artwork,
                ArtworkDecision::PreservedExistingTrack {
                    source_cover_bytes: Some(_),
                    ..
                }
            ) {
                existing_covers_unverified += 1;
            }
            continue;
        }
        if matches!(row.artwork, ArtworkDecision::NoSourceCoverPreserveExisting) {
            continue;
        }
        // A kept or updated track's cover was verified when it was sent, and
        // the catalogue still registers it; `verify` reads it back.
        if matches!(row.artwork, ArtworkDecision::VerifiedJournalMatch { .. })
            && matches!(row.decision, Decision::Keep | Decision::Update { .. })
        {
            continue;
        }
        let source = checked
            .sources
            .tracks
            .iter()
            .find(|s| s.relative_path == row.relative_path)
            .context("planned source missing")?;
        let binding = checked
            .journal
            .as_ref()
            .map(|j| j.lookup_active_verified(&source.fingerprint.audio))
            .transpose()?
            .flatten();
        let managed = matches!(row.decision, Decision::Add { .. }) || binding.is_some();
        if managed {
            // Add-only keeps an album's image unless the new member's cover
            // is identical. A replacement's adding sync (the only one with a
            // coordination) sends the new source's cover as a plain sync
            // does: the user replaces the track, cover included.
            let preserve_album = match &row.decision {
                Decision::Add { album, .. } if options.add_only && coordination.is_none() => {
                    let id = u64::from_str_radix(&album.id, 16)?;
                    protected_albums.contains(&id).then_some(id)
                }
                Decision::Keep | Decision::Update { .. } => binding
                    .as_ref()
                    .and_then(|binding| {
                        saved
                            .catalog
                            .tracks()
                            .iter()
                            .find(|t| t.sync_id == binding.content.track_pid)
                    })
                    .map(|t| t.album_sync_id)
                    .filter(|id| albums_with_unmanaged_tracks.contains(id)),
                _ => None,
            };
            // The fresh sync after the batches runs this row's other action
            // and its cover, and adds, with its cover, a new track the
            // batches leave out (an ALAC beside a batched MP3 at a batch size
            // of one): its cover cannot be checked before its audio is sent.
            let fresh = row.managed_audio_replacement.is_some()
                || (batched.is_some() && matches!(row.decision, Decision::Add { .. }))
                || !matches!(
                    row.decision,
                    Decision::Add { .. } | Decision::Keep | Decision::Update { .. }
                );
            covers.push((
                checked.sources.base_directory.join(&source.relative_path),
                source.fingerprint.clone(),
                preserve_album,
                fresh,
            ));
        } else {
            existing_covers_unverified += 1;
        }
    }
    let mut batched_covers = 0;
    if let Some(batched) = batched.as_mut() {
        let keep_album_image = options.add_only && coordination.is_none();
        for member in &mut batched.members {
            match batch_cover(
                protected_albums.contains(&member.album),
                matches!(member.audio, insert_batch::Audio::Alac { .. }),
                keep_album_image,
            ) {
                BatchCover::Carried => {}
                BatchCover::IfAlbumImage => member.keep_album_image = true,
                BatchCover::CoverStep => member.cover = None,
            }
        }
        batched_covers = batched.members.iter().filter(|m| m.cover.is_some()).count();
        let carried: Vec<_> = batched
            .members
            .iter()
            .map(|m| (m.source.as_path(), m.cover.is_some()))
            .collect();
        covers_after_batches(&mut covers, &carried);
    }
    if !covers.is_empty() || batched_covers > 0 {
        cover::require_decoder()?;
    }
    drop(checked);
    let mut covers_left_out = 0;
    let removed;
    let mut metadata_updates = match &mut batched {
        Some(batched) => {
            let added = add_in_batches(
                &options,
                coordination,
                engine,
                &saved,
                &journal,
                &run,
                &mut batched.members,
                batched.scope,
            )
            .await?;
            covers_left_out = added.covers_left_out;
            removed = added.removed;
            batched_covers -= covers_left_out + added.refused_covers;
            skipped.extend(added.refused);
            0
        }
        None => {
            // Removals sent alone change the phone under the plan: the sync
            // plans again without them, as when they must go first.
            if !options.removals.is_empty() {
                let removed =
                    Box::pin(remove_alone(&options, &saved, &journal, &run, engine)).await?;
                return Err(RemovedFirst(removed).into());
            }
            removed = 0;
            Box::pin(apply::run(
                apply::Options {
                    plan: &plan,
                    snapshot: &snapshot_path,
                    journal: &journal,
                    trace: &run.join("audio"),
                    host_name: options.host_name,
                    dry_run: false,
                },
                engine,
            ))
            .await?
            .metadata_updates()
        }
    };
    // This invocation's covers go before the fresh sync, which plans again
    // and so finds none of them pending.
    for (index, (source, fingerprint, preserve_album, _)) in covers.iter().enumerate() {
        engine.check()?;
        engine.emit(Event::CheckingCover {
            number: index + 1,
            total: covers.len(),
        })?;
        Box::pin(cover::run(
            cover::Options {
                file: source,
                snapshot: &snapshot_path,
                journal: &journal,
                trace: &run.join(format!("cover-{:04}", index + 1)),
                host_name: options.host_name,
                dry_run: false,
                expected_source: Some(fingerprint),
                preserve_album: *preserve_album,
                pending_cover_only: options.add_only,
            },
            engine,
        ))
        .await?;
    }
    if let Some(batched) = batched.as_ref().filter(|b| b.others > 0) {
        engine.emit(Event::SyncContinuing {
            remaining: batched.others,
        })?;
        // The fresh sync plans and sends the metadata updates as well.
        let rest = Box::pin(run_with(
            Options {
                snapshot: None,
                batch_size: 1,
                pinned: None,
                removals: &[],
                ..options
            },
            coordination,
            engine,
        ))
        .await?;
        // The fresh sync plans the same sources again.
        for left in rest.skipped {
            if !skipped.iter().any(|s| s.path == left.path) {
                skipped.push(left);
            }
        }
    } else if !updates.is_empty() {
        let directory = run.join("metadata");
        crate::engine::execution::native_insert::create_trace(&directory)?;
        metadata_updates += metadata_batch::update_all(
            &journal,
            &updates,
            coordination,
            &directory,
            options.host_name,
            || engine.control(options.seconds),
            |session, sessions| engine.emit(Event::MetadataBatchVerified { session, sessions }),
        )
        .await
        .with_context(|| {
            format!(
                "metadata updates stopped; evidence: {}",
                directory.display()
            )
        })?;
    }
    let mut completion = serde_json::json!({"schema":"rustunes.sync-completion",
        "managed_covers_verified":covers.len() + batched_covers,"existing_unmanaged_covers_unverified":existing_covers_unverified,
        "journal":journal,"plan":plan});
    if metadata_updates > 0 {
        completion["verified_metadata_updates"] = metadata_updates.into();
    }
    if covers_left_out > 0 {
        completion["add_only_covers_left_out"] = covers_left_out.into();
    }
    completion["skipped"] = serde_json::to_value(&skipped)?;
    private_write(
        &run.join("complete.json"),
        &serde_json::to_vec_pretty(&completion)?,
    )?;
    File::open(&run)?.sync_all()?;
    let mut result = SyncResult::new(
        SyncOutcome::Completed,
        &run,
        &plan,
        &journal,
        covers.len() + batched_covers,
        existing_covers_unverified,
    );
    result.metadata_updates_verified = metadata_updates;
    result.covers_left_out = covers_left_out;
    result.removed = removed;
    result.skipped = skipped;
    engine.emit(Event::SyncFinished(&result))?;
    Ok(result)
}

/// Where a batch member's cover goes.
#[derive(Debug, PartialEq)]
enum BatchCover {
    /// With its audio.
    Carried,
    /// With its audio once its batch proves the album's image is these exact
    /// bytes; otherwise nowhere.
    IfAlbumImage,
    /// By the cover step after the batches.
    CoverStep,
}

/// A cover rides with its audio unless its album was on the phone before.
/// Then an ALAC's goes by the cover step, which under add-only proves the
/// album's image unchanged. That step sends no other format's cover, so such
/// a cover rides as well, and the album keeps its image or takes the new one,
/// as the step allows. An add-only sync that must keep the album's image
/// sends it only when the album already shows exactly this cover, as the
/// step requires of an ALAC's.
fn batch_cover(album_existed: bool, alac: bool, keep_album_image: bool) -> BatchCover {
    match (album_existed, alac, keep_album_image) {
        (false, _, _) | (true, false, false) => BatchCover::Carried,
        (true, true, _) => BatchCover::CoverStep,
        (true, false, true) => BatchCover::IfAlbumImage,
    }
}

/// Keep the covers this invocation runs after its batches: a batched member's
/// when its batch did not carry it, and the pending cover of a row nothing
/// else runs (a kept track whose batch was cut short before its cover step).
/// A row the fresh sync afterwards handles (`fresh`) gets its cover there.
/// `carried` pairs each batched source with whether its batch carries its
/// cover.
fn covers_after_batches<T>(
    covers: &mut Vec<(PathBuf, T, Option<u64>, bool)>,
    carried: &[(&Path, bool)],
) {
    covers.retain(|(source, _, _, fresh)| {
        match carried
            .iter()
            .find(|(member, _)| *member == source.as_path())
        {
            Some((_, carried)) => !carried,
            None => !fresh,
        }
    });
}

/// What the batches left out, and what they removed.
struct Added {
    /// Covers left out to keep their albums' images.
    covers_left_out: usize,
    /// Tracks the removal left to the sync, now removed.
    removed: usize,
    /// FLACs not sent, and how many of them had a cover.
    refused: Vec<Skipped>,
    refused_covers: usize,
}

/// Insert the plan's new tracks in verified batches, each planned again from a
/// fresh capture under `run/audio/batch-NNN`. FLACs go as ALACs converted from
/// them in a private temporary directory, which also loses what earlier runs
/// left there unless an unsettled batch still needs it.
#[allow(clippy::too_many_arguments)]
async fn add_in_batches(
    options: &Options<'_>,
    coordination: Option<CoordinationId>,
    engine: &Engine,
    saved: &snapshot::Snapshot,
    journal: &Path,
    run: &Path,
    members: &mut [insert_batch::Member],
    scope: [u8; 32],
) -> Result<Added> {
    let identity = Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &music_protocol::catalogue::identity::parse(&saved.raw_preferences)?,
    )?;
    if !journal.join("journal.sqlite").try_exists()? {
        drop(Journal::create(journal, &identity)?);
    }
    let audio = run.join("audio");
    crate::engine::execution::native_insert::create_trace(&audio)?;
    let converter = if members.iter().any(|m| m.convert) {
        ensure!(
            options.flac_to_alac,
            "the plan converts FLACs, which needs --flac-to-alac"
        );
        let ffmpeg = crate::engine::execution::flac_to_alac::require_ffmpeg()?;
        // An outstanding input that cannot be read keeps every leftover.
        if let Ok(keep) =
            insert_batch::outstanding_sources(&Journal::open_read_only(journal, &identity)?)
        {
            crate::engine::execution::flac_to_alac::sweep(&keep)?;
        }
        let directory = crate::engine::execution::flac_to_alac::Directory::create()?;
        engine.emit(Event::ConvertingFlacs {
            directory: directory.path(),
        })?;
        Some(crate::engine::execution::flac_to_alac::Converter { ffmpeg, directory })
    } else {
        None
    };
    let mut added = Added {
        covers_left_out: 0,
        removed: 0,
        refused: Vec::new(),
        refused_covers: 0,
    };
    added.removed = insert_batch::add_all(
        journal,
        &identity,
        options.removals.to_vec(),
        members,
        scope,
        options.batch_size,
        coordination,
        &audio,
        options.host_name,
        || engine.control(options.seconds),
        converter.as_ref(),
        |settled, inserted, total| {
            engine.emit(Event::InsertBatchVerified {
                inserted,
                total,
                removed: settled.removed,
            })
        },
        |member| {
            added.covers_left_out += 1;
            engine.emit(Event::CoverLeftOut(&member.relative_path))
        },
        |member, error| {
            added.refused.push(Skipped {
                path: member.relative_path.clone(),
                code: "flac_conversion_failed",
                reason: "its conversion to ALAC failed its check (FFmpeg failed, or the ALAC did not decode to the FLAC's exact audio), so it was not sent. Check that the FLAC decodes cleanly, for example with `rustunes validate --decode`.".into(),
                detail: format!("{error:#}"),
                stage: SkipStage::Send,
            });
            added.refused_covers += usize::from(member.cover.is_some());
            engine.emit(Event::FlacRefused {
                source: &member.relative_path,
                reason: format!("{error:#}"),
            })
        },
    )
    .await
    .with_context(|| format!("batched insertion stopped; evidence: {}", audio.display()))?
    .1;
    if let Some(converter) = converter {
        converter.directory.finish()?;
    }
    Ok(added)
}

/// Send the tracks the removal left to this sync in removal batches of their
/// own, under `run/removal`, when no insert batch takes them. Returns how
/// many were removed.
async fn remove_alone(
    options: &Options<'_>,
    saved: &snapshot::Snapshot,
    journal: &Path,
    run: &Path,
    engine: &Engine,
) -> Result<usize> {
    if options.removals.is_empty() {
        return Ok(0);
    }
    let identity = Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &music_protocol::catalogue::identity::parse(&saved.raw_preferences)?,
    )?;
    let directory = run.join("removal");
    crate::engine::execution::native_insert::create_trace(&directory)?;
    let settled = crate::engine::execution::removal_batch::remove_all(
        journal,
        &identity,
        options.removals,
        options.batch_size,
        None,
        &directory,
        options.host_name,
        || engine.control(options.seconds),
        |_, _, _| Ok(()),
    )
    .await
    .with_context(|| {
        format!(
            "the removal of the tracks left to this sync stopped; evidence: {}",
            directory.display()
        )
    })?;
    Ok(settled.iter().map(|b| b.removed).sum())
}

/// The refusals that hold `plan`, each with its plain reason.
fn holds(plan: &crate::engine::planning::additive::Plan) -> Vec<Hold> {
    let mut holds: Vec<_> = plan
        .journal_blockers
        .iter()
        .map(|blocker| Hold {
            path: None,
            code: blocker.clone(),
            reason: journal_reason(blocker),
        })
        .collect();
    for row in &plan.sources {
        if let Decision::Conflict { code, message } = &row.decision {
            holds.push(Hold {
                path: Some(row.relative_path.clone()),
                code: (*code).into(),
                reason: crate::engine::planning::additive::skip::reason(
                    row,
                    code,
                    message,
                    &plan.sources,
                ),
            });
        }
    }
    holds
}

/// A journal blocker (`additive::Plan::journal_blockers`) in plain words.
fn journal_reason(blocker: &str) -> String {
    match blocker {
        "outstanding_prepared_requires_resolution" => {
            "an earlier run prepared a change it never started; `rustunes status` shows it and how to cancel it.".into()
        }
        "outstanding_started_requires_reconciliation" => {
            "an earlier run started a change it didn't finish; `rustunes status` shows it and how to settle it.".into()
        }
        _ => match blocker.strip_prefix("verified_binding_drift:") {
            Some(track) => format!(
                "track {track}, which rustunes sent, no longer matches what it sent; `rustunes status` shows it."
            ),
            None => format!("rustunes' journal has unfinished work ({blocker}); `rustunes status` shows it."),
        },
    }
}

/// How to send the rest of a blocked sync without the sources that hold it:
/// only when sources alone hold it, and outside a replacement or saved plan,
/// which go on only as they began.
fn hint(
    holds: &[Hold],
    options: &Options<'_>,
    coordination: Option<CoordinationId>,
    connection: &str,
) -> Option<String> {
    // Refusals that hold every source alike.
    const WHOLE_RUN: [&str; 2] = ["journal_reconciliation_required", "mp3_journal_required"];
    let paths: Vec<_> = holds
        .iter()
        .map(|h| h.path.as_deref())
        .collect::<Option<_>>()?;
    if paths.is_empty()
        || coordination.is_some()
        || options.pinned.is_some()
        || holds.iter().any(|h| WHOLE_RUN.contains(&h.code.as_str()))
    {
        return None;
    }
    let excludes = paths
        .iter()
        .map(|path| {
            format!(
                "--exclude {}",
                crate::engine::execution::replace::quoted(path)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(if options.add_only {
        format!("To send the rest now, push again with {excludes}.")
    } else {
        format!(
            "To send the rest now without them, run `rustunes push{}{connection} {excludes}`: push sends new tracks only, and removes and updates nothing.",
            if options.selection.recursive {
                " --recursive"
            } else {
                ""
            }
        )
    })
}

/// Called only after the saved plan has been regenerated from the complete
/// snapshot and fresh source inspection. An MPEG Keep or Update has no cover
/// path: an exact file is kept as it is, and an owned file matched by its
/// frames keeps its audio and cover while its fields follow the source's tags
/// (sent with the metadata updates), as does a FLAC's ALAC this tool converted
/// and sent. Returns whether the source has a cover this run leaves
/// unverified.
fn preserved_mpeg_keep(
    row: &crate::engine::planning::additive::SourceDecision,
    sources: &crate::engine::planning::source_collection::SourceCollection,
) -> Result<Option<bool>> {
    let Some(mpeg) = &row.mp3 else {
        return Ok(None);
    };
    let retagged =
        matches!(&row.matches[..], [m] if m.match_basis == Some("owned_mpeg_frame_identity"));
    let converted = matches!(&row.matches[..], [m] if m.match_basis == Some("flac_conversion"));
    let owned = retagged || converted;
    ensure!(
        (matches!(row.decision, Decision::Keep)
            || owned && matches!(row.decision, Decision::Update { .. }))
            && (row.projected.is_none() || owned)
            && row.native_composer.is_none(),
        "MPEG sync only supports preserved Keep and owned Update decisions"
    );
    let source = sources
        .mp3_tracks
        .iter()
        .find(|s| s.relative_path == row.relative_path)
        .context("planned MPEG source missing")?;
    ensure!(
        &source.source == mpeg
            && row.source_file_bytes == Some(mpeg.fingerprint.file_bytes)
            && row.source_file_sha256.as_ref() == Some(&mpeg.fingerprint.full_file_sha256)
            && row.encoded_audio_sha256.as_ref() == Some(&mpeg.fingerprint.audio.sha256),
        "planned MPEG source facts differ"
    );
    ensure!(
        row.matches.len() == 1
            && row.matches[0].codec
                == if converted {
                    music_protocol::media::format::Format::Alac.codec_label()
                } else {
                    mpeg.format().codec_label()
                }
            && matches!(row.matches[0].ownership, "unowned" | "inserted_by_us")
            && row.matches[0].encoded_audio_equal != converted
            && if owned {
                row.matches[0].ownership == "inserted_by_us"
                    && !row.matches[0].full_container_equal
                    && row.matches[0].device_file_sha256.is_some()
            } else {
                row.matches[0].full_container_equal
                    && row.matches[0].match_basis == Some("full_file_equality_with_mpeg_layout")
            },
        "MPEG Keep lacks exact file evidence"
    );
    let ArtworkDecision::PreservedExistingTrack {
        source_cover_sha256,
        source_cover_bytes,
        ..
    } = &row.artwork
    else {
        anyhow::bail!("MPEG Keep cannot schedule an artwork action")
    };
    let cover = music_protocol::media::mpeg::cover_picture(&mpeg.artwork);
    ensure!(
        source_cover_sha256.as_ref() == cover.map(|p| &p.sha256)
            && *source_cover_bytes == cover.map(|p| p.bytes),
        "MPEG Keep cover facts differ"
    );
    Ok(Some(source_cover_bytes.is_some() && !owned))
}
#[cfg(test)]
mod tests {
    #[test]
    fn only_a_batch_undone_cleanly_plans_again() {
        let reason = "insert batch 7 added 96 of its 100 tracks";
        let cut = anyhow::Error::new(insert_batch::CutShort(reason.into(), 3))
            .context("batched insertion stopped; evidence: /run/audio");
        assert_eq!(
            cut_short(&cut).map(|c| (c.0.as_str(), c.1)),
            Some((reason, 3))
        );
        assert!(format!("{cut:#}").ends_with("run sync again for the rest"));
        let other = anyhow::anyhow!("insert batch 7 ended Started").context("stopped");
        assert!(cut_short(&other).is_none());
    }
    #[test]
    fn only_an_album_on_the_phone_keeps_a_cover_from_riding_freely() {
        // (album existed, ALAC, keep the album's image)
        for (case, route) in [
            ((false, true, true), BatchCover::Carried),
            ((false, false, true), BatchCover::Carried),
            ((true, true, false), BatchCover::CoverStep),
            ((true, true, true), BatchCover::CoverStep),
            ((true, false, false), BatchCover::Carried),
            ((true, false, true), BatchCover::IfAlbumImage),
        ] {
            assert_eq!(batch_cover(case.0, case.1, case.2), route, "{case:?}");
        }
    }
    #[test]
    fn a_kept_tracks_pending_cover_runs_after_the_batches() {
        let path = |name: &str| PathBuf::from(format!("/music/{name}.m4a"));
        let mut covers = vec![
            // Batched, and its batch carries the cover.
            (path("carried"), (), None, false),
            // Batched without its cover: its album was on the phone before.
            (path("stripped"), (), Some(9), false),
            // Kept: a batch cut short before its cover step left it pending.
            (path("kept"), (), None, false),
            // A managed audio replacement, which the fresh sync afterwards runs.
            (path("replaced"), (), None, true),
        ];
        let (carried, stripped) = (path("carried"), path("stripped"));
        covers_after_batches(
            &mut covers,
            &[(carried.as_path(), true), (stripped.as_path(), false)],
        );
        assert_eq!(
            covers.iter().map(|c| c.0.clone()).collect::<Vec<_>>(),
            [path("stripped"), path("kept")]
        );
    }

    use super::*;
    use crate::{
        engine::planning::additive::{ExistingMatch, SourceDecision},
        engine::planning::source_collection,
    };
    #[test]
    fn exact_mpeg_keep_bypasses_alac_cover_lookup_without_claiming_ownership() {
        let path = std::env::temp_dir().join(format!(
            "mpeg-sync-route-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        std::fs::write(path.join("song.mp3"), [frame.clone(), frame].concat()).unwrap();
        let sources = source_collection::inspect(
            &source_collection::select(&path, &SelectionOptions::default()).unwrap(),
        )
        .unwrap();
        assert!(sources.tracks.is_empty());
        let mp3 = sources.mp3_tracks[0].source.clone();
        let mut row = SourceDecision {
            relative_path: "song.mp3".into(),
            managed_audio_replacement: None,
            source_file_bytes: Some(mp3.fingerprint.file_bytes),
            source_file_sha256: Some(mp3.fingerprint.full_file_sha256.clone()),
            encoded_audio_sha256: Some(mp3.fingerprint.audio.sha256.clone()),
            projected: None,
            mp3: Some(mp3),
            mp3_insertion: None,
            flac_to_alac: false,
            validation_issues: Vec::new(),
            matches: vec![ExistingMatch {
                track_id: "0000000000001234".into(),
                wire_id: "4660".into(),
                path: "iTunes_Control/Music/F00/TEST.mp3".into(),
                codec: "mpeg1_layer3",
                encoded_audio_equal: true,
                full_container_equal: true,
                match_basis: Some("full_file_equality_with_mpeg_layout"),
                ownership: "unowned",
                catalog_differences: Vec::new(),
                canonicalized_fields: Vec::new(),
                device_file_sha256: None,
            }],
            artwork: ArtworkDecision::PreservedExistingTrack {
                source_cover_sha256: None,
                source_cover_bytes: None,
                observed_cache_ids: Vec::new(),
            },
            native_composer: None,
            decision: Decision::Keep,
        };
        assert_eq!(preserved_mpeg_keep(&row, &sources).unwrap(), Some(false));
        row.matches[0].ownership = "inserted_by_us";
        assert_eq!(preserved_mpeg_keep(&row, &sources).unwrap(), Some(false));
        row.matches[0].ownership = "unproven";
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.matches[0].ownership = "unowned";
        row.matches[0].full_container_equal = false;
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.matches[0].full_container_equal = true;
        // An Update only for an owned file matched by its frames.
        row.decision = Decision::Update {
            track_id: "0000000000001234".into(),
            changes: Vec::new(),
        };
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.matches[0].match_basis = Some("owned_mpeg_frame_identity");
        row.matches[0].full_container_equal = false;
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.matches[0].ownership = "inserted_by_us";
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.matches[0].device_file_sha256 = Some("1".repeat(64));
        assert_eq!(preserved_mpeg_keep(&row, &sources).unwrap(), Some(false));
        row.decision = Decision::Keep;
        assert_eq!(preserved_mpeg_keep(&row, &sources).unwrap(), Some(false));
        row.artwork = ArtworkDecision::PendingOwnedCover {
            sha256: "0".repeat(64),
            bytes: 1,
        };
        assert!(preserved_mpeg_keep(&row, &sources).is_err());
        row.mp3 = None;
        assert_eq!(preserved_mpeg_keep(&row, &sources).unwrap(), None);
        std::fs::remove_dir_all(path).unwrap();
    }
}
