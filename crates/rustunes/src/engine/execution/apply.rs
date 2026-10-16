//! `sync`'s one-track sessions: apply the additive plan it saved, exactly
//! reconstructed, one journaled track at a time.
//! Each accepted poststate becomes the next step's prestate. No deletion API is
//! reachable here, and interrupted Started attempts are never retried.
use std::{
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::{
    engine::execution::device,
    engine::execution::insert,
    engine::execution::native_insert::{self, Control, FreshState, Insertion},
    engine::planning::additive::Decision,
    engine::planning::collections::{CandidateIds, Planner},
    engine::planning::saved_plan,
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::state::current::{self, hex},
    engine::state::journal::{AttemptKind, CompletionEvidence, Content, Journal, PreparedAttempt},
    engine::{ApplyResult, Engine, Event},
};
use music_protocol::catalogue::insert_verification::{self, ExpectedInsert};
use music_protocol::protocol::templates;
use music_protocol::protocol::trace::private_write;

mod refresh;

mod sequence;

pub struct Options<'a> {
    pub plan: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub trace: &'a Path,
    pub host_name: &'a str,
    pub dry_run: bool,
}

pub(crate) async fn run(options: Options<'_>, engine: &Engine) -> Result<ApplyResult> {
    engine.check()?;
    ensure!(
        !options.trace.try_exists()?,
        "apply trace directory must be new"
    );
    let long_control = engine.control(1800);
    let mut validated = saved_plan::load_read_only_with_control(
        options.plan,
        options.snapshot,
        options.journal,
        &long_control,
    )?;
    // Exact description eligibility is still separate from physical/native authority.
    ensure_supported_additions(&validated.plan.sources, &validated.sources, options.dry_run)?;
    let additions = validated
        .plan
        .sources
        .iter()
        .filter(|r| matches!(r.decision, Decision::Add { .. }))
        .count();
    // Sent afterwards in metadata batches by `sync`.
    let metadata_updates = validated
        .plan
        .sources
        .iter()
        .filter(|r| matches!(r.decision, Decision::Update { .. }))
        .count();
    fs::DirBuilder::new().mode(0o700).create(options.trace)?;
    let canonical_trace = fs::canonicalize(options.trace)?;
    let options = Options {
        trace: &canonical_trace,
        ..options
    };
    File::open(
        options
            .trace
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    let current_directory = options.trace.join("before");
    let current = current::capture_controlled(
        &current_directory,
        &validated.identity,
        engine.control(120).deadline,
        Arc::clone(&long_control.cancelled),
    )
    .await?;
    ensure!(
        current.catalog.source_hashes().database
            == validated.snapshot.catalog.source_hashes().database
            && current.catalog.source_hashes().wal
                == validated.snapshot.catalog.source_hashes().wal
            && current.catalog.persisted_media_anchor() == validated.plan.media_anchor
            && current.raw_preferences == validated.snapshot.raw_preferences
            && current.plist_preferences == validated.snapshot.plist_preferences,
        "the phone's library or preferences changed after planning (playing music changes it, and the phone also renumbers its library on its own a few minutes after a sync); no transfer started, take a fresh snapshot and plan again"
    );
    // Qualify all runtime template fields before creating a journal or reservation.
    templates::build_from_copied_cdb(
        &current.cdb,
        &current.catalog,
        &current.library,
        options.host_name,
    )?;
    if options.dry_run {
        let receipt = serde_json::json!({"schema":"rustunes.apply-preflight",
            "plan_sha256":hex(&validated.plan_sha256),"current_state_receipt_sha256":hex(&current.receipt_sha256),
            "additions":additions,"phone_writes":0,"journal_created":false,"saved_plan_matches_current_catalogue":true});
        private_write(
            &options.trace.join("preflight.json"),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        File::open(options.trace)?.sync_all()?;
        let result = if metadata_updates > 0 {
            ApplyResult::PreflightSourceChanges {
                additions,
                metadata_updates,
            }
        } else {
            ApplyResult::Preflight { additions }
        };
        engine.emit(Event::ApplyFinished(result))?;
        return Ok(result);
    }
    if additions == 0 {
        engine.emit(Event::ApplyFinished(ApplyResult::Unchanged))?;
        return Ok(ApplyResult::Unchanged);
    }
    let journal = match validated.journal.take() {
        Some(reader) => {
            drop(reader);
            Journal::open(options.journal, &current.identity)?
        }
        None => Journal::create(options.journal, &current.identity)?,
    };
    journal.ensure_identity(&current.identity)?;
    ensure!(
        journal.outstanding()?.is_none(),
        "journal has an unfinished operation; reconcile it before applying"
    );
    let scope = crate::engine::planning::removal::collection_scope_sha256(&validated.sources)?;
    let context = StepContext {
        engine,
        validated: &validated,
        options: &options,
        scope,
        additions,
        long_control: &long_control,
    };
    let state = ApplyState {
        journal,
        current,
        current_directory,
    };
    sequence::run(
        &validated.plan.sources,
        state,
        options.trace,
        validated.plan_sha256,
        |state, row, number| apply_step(state, row, number, &context),
        |state| {
            (
                state.current.catalog.persisted_media_anchor(),
                state.current.catalog.tracks().len(),
            )
        },
    )
    .await?;
    let result = ApplyResult::Completed { additions };
    engine.emit(Event::ApplyFinished(result))?;
    Ok(result)
}

struct ApplyState {
    journal: Journal,
    current: current::Current,
    current_directory: std::path::PathBuf,
}
struct StepContext<'a> {
    engine: &'a Engine,
    validated: &'a saved_plan::Validated,
    options: &'a Options<'a>,
    scope: [u8; 32],
    additions: usize,
    long_control: &'a Control,
}
async fn apply_step(
    state: ApplyState,
    row: &crate::engine::planning::additive::SourceDecision,
    number: usize,
    context: &StepContext<'_>,
) -> Result<sequence::StepResult<ApplyState>> {
    let ApplyState {
        mut journal,
        mut current,
        mut current_directory,
    } = state;
    let StepContext {
        engine,
        validated,
        options,
        scope,
        additions,
        long_control,
    } = *context;
    engine.check()?;
    let plan_sha256 = validated.plan_sha256;
    let index_sha256 = validated.snapshot.index_receipt_sha256;
    let Decision::Add {
        track_id,
        album,
        artist,
        path,
        ..
    } = &row.decision
    else {
        anyhow::bail!("apply step requires an Add row");
    };
    let step_name = sequence::step_name(number);
    let step = options.trace.join(&step_name);
    let track_id = u64::from_str_radix(track_id, 16)?;
    let album_id = u64::from_str_radix(&album.id, 16)?;
    let artist_id = u64::from_str_radix(&artist.id, 16)?;
    let source = validated
        .sources
        .tracks
        .iter()
        .find(|s| s.relative_path == row.relative_path)
        .context("planned source inspection missing")?;
    let source_path = validated.sources.base_directory.join(&source.relative_path);
    ensure!(
        music_protocol::media::fingerprint::read(&source_path)? == source.fingerprint,
        "source changed after saved-plan validation"
    );
    fs::DirBuilder::new().mode(0o700).create(&step)?;
    let candidates = CandidateIds {
        track: track_id,
        album: Some(album_id),
        artist: Some(artist_id),
    };
    let operation_timestamp = SystemTime::now();
    let mut planner = Planner::new(&current.catalog, &journal.reserved_pids()?);
    let selected = planner.plan(&source.metadata, candidates)?;
    let operation = insert::prepare_live(
        &source.metadata,
        &selected,
        &current.catalog,
        operation_timestamp,
    )?;
    ensure!(
        selected.album_pid() == album_id
            && selected.artist_pid() == artist_id
            && selected.pending_collection_ids().is_empty(),
        "fresh grouping differs from saved plan; no alternate IDs will be substituted"
    );
    verify_planned_composer(&operation, row, &source.metadata)?;
    // Keep a durable exact source for recovery if the caller later changes
    // the source directory while this operation is awaiting verification.
    let control = engine.control(180);
    retain_alac_source(&source_path, &source.fingerprint, &step, &control)?;
    let input = serde_json::json!({"schema":"rustunes.insertion-input",
        "plan_sha256":hex(&plan_sha256),"complete_index_sha256":hex(&index_sha256),
        "current_state_receipt_sha256":hex(&current.receipt_sha256),"source":row.relative_path,
        "full_source_sha256":hex(&source.fingerprint.full_file_sha256),"encoded_audio_sha256":hex(&source.fingerprint.audio.sha256),
        "track_id":format!("{track_id:016x}"),"album_id":format!("{album_id:016x}"),"artist_id":format!("{artist_id:016x}"),
        "path":path,"operation_sha256":hex(&Sha256::digest(&operation)),"phone_deletions":0});
    let mut prepared = PreparedAttempt {
        kind: AttemptKind::Insert,
        content: Content {
            source: (source.fingerprint.clone()).into(),
            track_pid: track_id,
            path: path.clone(),
            artwork: None,
            collection: Some(scope),
        },
        new_album_pid: selected
            .new_collection_ids()
            .contains(&album_id)
            .then_some(album_id),
        new_artist_pid: selected
            .new_collection_ids()
            .contains(&artist_id)
            .then_some(artist_id),
        current_media_anchor: current.catalog.persisted_media_anchor(),
        operation,
        input_receipt_sha256: [0; 32],
    };
    engine.emit(Event::AdditionStarted {
        number,
        total: additions,
        source: &row.relative_path,
    })?;
    let pending = ordinary_insert(
        &mut journal,
        &mut current,
        OrdinaryInsert {
            engine,
            current_directory: &current_directory,
            step: &step,
            sources: &validated.sources,
            source,
            source_path: &source_path,
            prepared: &mut prepared,
            input,
            row,
            selected: &selected,
            timestamp: operation_timestamp,
            host_name: options.host_name,
        },
        &control,
    )
    .await
    .context(
        "insertion did not reach independent verification; inspect the journal before any retry",
    )?;
    let after_directory = step.join("after");
    let after = current::capture_controlled(
        &after_directory,
        &current.identity,
        engine.control(120).deadline,
        Arc::clone(&long_control.cancelled),
    )
    .await
    .context("transfer returned but poststate capture failed; journal requires reconciliation")?;
    let checked = insert_verification::verify_insert(
        &current.catalog,
        &after.catalog,
        &ExpectedInsert {
            track_id,
            album_id,
            artist_id,
            path,
            file_bytes: source.fingerprint.file_bytes,
            metadata: (&source.metadata).into(),
            presented: true,
        },
    );
    let checked = checked
        .context("post-transfer catalogue verification failed; journal requires reconciliation")?;
    let receipt = serde_json::json!({"schema":"rustunes.verified-insertion","catalogue":checked,
        "plan_sha256":hex(&plan_sha256),"transfer_receipt_sha256":hex(&pending.evidence.receipt_sha256),
        "after_state_receipt_sha256":hex(&after.receipt_sha256),"permanent_audio_sha256":hex(&pending.evidence.permanent_audio_sha256),
        "source":row.relative_path,"audio_verified":true,"artwork_verified":false,"phone_deletions":0});
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    private_write(&step.join("verified.json"), &bytes)?;
    File::open(&step)?.sync_all()?;
    journal.complete(
        pending.permit,
        CompletionEvidence {
            track_pid: track_id,
            path: path.clone(),
            audio_bytes: source.fingerprint.file_bytes,
            audio_file_sha256: pending.evidence.permanent_audio_sha256,
            artwork_sha256: None,
            media_anchor: after.catalog.persisted_media_anchor(),
            receipt_sha256: Sha256::digest(&bytes).into(),
        },
    )?;
    journal.record_source(track_id, &source_path)?;
    let receipt = serde_json::json!({"source":row.relative_path,"track_id":format!("{track_id:016x}"),"receipt_sha256":hex(&Sha256::digest(&bytes))});
    current = after;
    current_directory = after_directory;
    engine.emit(Event::AudioInsertionVerified)?;
    Ok(sequence::StepResult {
        state: ApplyState {
            journal,
            current,
            current_directory,
        },
        receipt,
    })
}

// Retain exact transferred tag/container bytes once before input, intent or
// native dispatch. The transfer still reads and checks the caller's original
// path.
fn retain_alac_source(
    source_path: &Path,
    fingerprint: &music_protocol::media::fingerprint::SourceFingerprint,
    step: &Path,
    control: &Control,
) -> Result<()> {
    control.check()?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(step)?;
    let held_directory = directory.metadata()?;
    ensure!(
        held_directory.permissions().mode() & 0o777 == 0o700,
        "ALAC retention step is not private"
    );
    let retained = step.join("retained-source.m4a");
    let mut bound = native_insert::BoundSource::open(source_path, fingerprint)?;
    control.check()?;
    let bytes = bound.read_for_retention()?;
    control.check()?;
    private_write(&retained, &bytes)?;
    control.check()?;
    let retained_bound = native_insert::BoundSource::open(&retained, fingerprint)?;
    control.check()?;
    ensure!(
        music_protocol::media::fingerprint::read(&retained)? == *fingerprint,
        "source changed while retaining ALAC original bytes"
    );
    control.check()?;
    bound.check()?;
    finish_alac_retention(
        step,
        &directory,
        &held_directory,
        &bound,
        &retained_bound,
        control,
    )
}

// Finalize the same retention operation after the typed fingerprint read.
fn finish_alac_retention(
    step: &Path,
    directory: &File,
    held_directory: &fs::Metadata,
    bound: &native_insert::BoundSource,
    retained_bound: &native_insert::BoundSource,
    control: &Control,
) -> Result<()> {
    let retained = step.join("retained-source.m4a");
    let retained_metadata = fs::symlink_metadata(&retained)?;
    ensure!(
        retained_metadata.is_file()
            && retained_metadata.nlink() == 1
            && retained_metadata.permissions().mode() & 0o777 == 0o600,
        "retained ALAC original is not a private unaliased file"
    );
    let current_directory = fs::symlink_metadata(step)?;
    ensure!(
        current_directory.is_dir()
            && current_directory.dev() == held_directory.dev()
            && current_directory.ino() == held_directory.ino(),
        "ALAC retention step changed"
    );
    directory.sync_all()?;
    bound.check()?;
    retained_bound.check()?;
    control.check()?;
    Ok(())
}

// Keep this boundary ahead of trace creation and any device capture: it keeps
// an MP3 source out of the ALAC-only apply body.
/// Every MP3 addition goes in an insert batch. Apply sends one track per
/// session, so a real run refuses it; the preflight of `sync --dry-run` checks
/// it against the insert-batch profile.
fn ensure_supported_additions(
    rows: &[crate::engine::planning::additive::SourceDecision],
    sources: &crate::engine::planning::source_collection::SourceCollection,
    dry_run: bool,
) -> Result<()> {
    for row in rows {
        if !matches!(row.decision, Decision::Add { .. }) {
            continue;
        }
        if row.mp3.is_some()
            || row.mp3_insertion.is_some()
            || sources
                .mp3_tracks
                .iter()
                .any(|source| source.relative_path == row.relative_path)
        {
            let source = sources
                .mp3_tracks
                .iter()
                .find(|s| s.relative_path == row.relative_path)
                .context("planned MPEG source missing")?;
            ensure!(
                row.mp3.as_ref() == Some(&source.source),
                "planned MPEG inspection differs from selected source"
            );
            ensure!(
                dry_run,
                "{} goes in an insert batch, which sync sends; apply sends one track per session",
                row.relative_path
            );
            PreparedSource::read_expected(
                &sources.base_directory.join(&source.relative_path),
                &source.source,
            )?
            .ensure_batch_insertion_allowed()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod codec_gate_tests {
    use super::*;
    use crate::engine::planning::additive::{ArtworkDecision, Group, SourceDecision};
    #[test]
    fn mpeg_add_cannot_route_through_alac_apply_even_without_report_fields() {
        let path = std::env::temp_dir().join(format!(
            "apply-mpeg-gate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let mut frame = vec![0; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        fs::write(path.join("source.mp3"), [frame.clone(), frame].concat()).unwrap();
        let sources = crate::engine::planning::source_collection::inspect(
            &crate::engine::planning::source_collection::select(
                &path,
                &crate::engine::planning::source_collection::SelectionOptions::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let group = || Group {
            id: "0000000000001234".into(),
            wire_id: "4660".into(),
            status: "new",
        };
        let mut row = SourceDecision {
            relative_path: "source.mp3".into(),
            managed_audio_replacement: None,
            source_file_bytes: None,
            source_file_sha256: None,
            encoded_audio_sha256: None,
            projected: None,
            mp3: None,
            mp3_insertion: None,
            flac_to_alac: false,
            validation_issues: Vec::new(),
            matches: Vec::new(),
            artwork: ArtworkDecision::NoSourceCoverPreserveExisting,
            native_composer: None,
            decision: Decision::Add {
                track_id: "0000000000001235".into(),
                wire_id: "4661".into(),
                album: group(),
                artist: group(),
                path: "iTunes_Control/Music/F00/TEST.mp3".into(),
                pending_collection_ids: Vec::new(),
                fresh_state_required_before_execution: true,
            },
        };
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_err());
        // An omitted or mislabelled codec field cannot bypass actual selection.
        row.mp3 = Some(sources.mp3_tracks[0].source.clone());
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_err());
        row.decision = Decision::Keep;
        assert!(ensure_supported_additions(std::slice::from_ref(&row), &sources, false).is_ok());
        fs::remove_dir_all(path).unwrap();
    }
}

fn verify_planned_composer(
    operation: &[u8],
    row: &crate::engine::planning::additive::SourceDecision,
    source: &music_protocol::media::metadata::Metadata,
) -> Result<()> {
    let plist = plist::Value::from_reader(std::io::Cursor::new(operation))?;
    let item = plist
        .as_dictionary()
        .and_then(|r| r.get("operations"))
        .and_then(plist::Value::as_array)
        .and_then(|ops| ops.last())
        .and_then(plist::Value::as_dictionary)
        .and_then(|r| r.get("item"))
        .and_then(plist::Value::as_dictionary)
        .context("generated insertion item missing")?;
    let source_composer = source.projected()?.composer;
    let (label, sort) = row
        .native_composer
        .as_ref()
        .map(|p| (Some(p.wire_label.as_str()), Some(p.wire_sort.as_str())))
        .unwrap_or((source_composer, source_composer));
    ensure!(
        item.get("composer").and_then(plist::Value::as_string) == label
            && item.get("sort_composer").and_then(plist::Value::as_string) == sort,
        "composer grouping changed after planning; no alternate metadata projection will be substituted"
    );
    Ok(())
}

fn insertion_state(current: &current::Current, input_hash: [u8; 32]) -> Result<FreshState> {
    let mut state = FreshState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        input_hash,
    )?;
    state.bind_copied_cdb(&current.cdb)?;
    Ok(state)
}

struct OrdinaryInsert<'a> {
    engine: &'a Engine,
    current_directory: &'a Path,
    step: &'a Path,
    sources: &'a crate::engine::planning::source_collection::SourceCollection,
    source: &'a crate::engine::planning::source_collection::SourceTrack,
    source_path: &'a Path,
    prepared: &'a mut PreparedAttempt,
    input: serde_json::Value,
    row: &'a crate::engine::planning::additive::SourceDecision,
    selected: &'a crate::engine::planning::collections::PlannedTrack,
    timestamp: SystemTime,
    host_name: &'a str,
}

async fn ordinary_insert(
    journal: &mut Journal,
    current: &mut current::Current,
    mut i: OrdinaryInsert<'_>,
    control: &Control,
) -> Result<native_insert::PendingVerification> {
    let frozen = refresh::FrozenIntent::new(i.prepared, i.selected, i.timestamp);
    for attempt in 1..=2u8 {
        control.check()?;
        let preflight = i.step.join(format!("preflight-{attempt:04}"));
        fs::DirBuilder::new().mode(0o700).create(&preflight)?;
        let before = preflight.join("before");
        if attempt == 1 {
            refresh::copy_before(i.current_directory, &before, current, control)?;
        } else {
            let fresh = refresh::controlled(
                current::capture_until(&before, &current.identity, control.deadline),
                control,
            )
            .await?;
            refresh::ensure_current(current, &fresh)?;
            refresh::ensure_sources(i.sources, control)?;
            let rebuilt = frozen.revalidate(
                i.prepared,
                i.source,
                &fresh.catalog,
                &journal.reserved_pids()?,
                control,
            )?;
            verify_planned_composer(&rebuilt, i.row, &i.source.metadata)?;
            *current = fresh;
        }
        control.check()?;
        i.input["pre_session_attempt"] = attempt.into();
        i.input["current_state_receipt_sha256"] = hex(&current.receipt_sha256).into();
        let input_bytes = serde_json::to_vec_pretty(&i.input)?;
        private_write(&preflight.join("input.json"), &input_bytes)?;
        File::open(&preflight)?.sync_all()?;
        File::open(i.step)?.sync_all()?;
        File::open(i.step.parent().context("insertion trace parent missing")?)?.sync_all()?;
        i.prepared.input_receipt_sha256 = Sha256::digest(&input_bytes).into();
        let state = insertion_state(current, i.prepared.input_receipt_sha256)?;
        let templates = templates::build_from_copied_cdb(
            &current.cdb,
            &current.catalog,
            &current.library,
            i.host_name,
        )?;
        let result = native_insert::execute_new_native_insert(
            journal,
            Insertion {
                attempt: i.prepared,
                source: i.source_path,
                state: &state,
                templates,
                trace: &preflight.join("transfer"),
            },
            &i.step.join("input.json"),
            &input_bytes,
            control,
            device::provider,
        )
        .await;
        match result {
            Ok(pending) => return Ok(pending),
            Err(error) => {
                let refresh = refresh::may_refresh(&error, attempt, control)?;
                private_write(
                    &preflight.join("failure.json"),
                    &serde_json::to_vec_pretty(
                        &serde_json::json!({"schema":"rustunes.insertion-pre-session-failure",
                    "input_receipt_sha256":hex(&i.prepared.input_receipt_sha256),
                    "before_receipt_sha256":hex(&current.receipt_sha256),
                    "failure":format!("{error:#}"), "read_only_refresh_selected":refresh,
                    "deadline_renewed":false}),
                    )?,
                )?;
                File::open(&preflight)?.sync_all()?;
                if !refresh {
                    return Err(error);
                }
                ensure!(
                    journal.outstanding()?.is_none() && !i.step.join("input.json").try_exists()?,
                    "pre-session refresh cannot follow a journal attempt or published input"
                );
                i.engine.emit(Event::RefreshingBeforeSession)?;
            }
        }
    }
    anyhow::bail!("pre-session refresh limit exhausted")
}

#[cfg(test)]
mod basic_tests;
