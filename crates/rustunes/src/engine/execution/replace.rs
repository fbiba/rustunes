//! Coordinated replacement: remove tracks from the phone, prove they are
//! gone, then add the sources again with the ordinary sync, all under one
//! journal coordination.
//!
//! - `sync DIR --replace-library` (ReplaceLibrary) removes every track on the
//!   phone and adds the selected sources again.
//! - `sync DIR --replace-changed` (ReplaceTrack) removes the tracks this tool
//!   inserted whose source audio has since changed at the same path, and adds
//!   the new audio.
//!
//! The coordination's phase records progress: Removing, then Adding once a
//! fresh capture shows none of the selected tracks. Both phases work from a
//! fresh capture, so running the same command again continues an Active
//! coordination: removal takes out what is left and the sync adds only what
//! is missing. The coordination completes only when a fresh add-only plan
//! has nothing left to add and no cover of an added track pending. An
//! unfinished batch needs `resume` first.
//!
//! A whole-library replacement starts only when this tool can add every
//! selected source; one it never could would leave the phone short after the
//! removal. Since that removal takes every track whatever the selection, an
//! Active one may continue with more `--exclude` paths than it began with.
//! Removed tracks lose their identities, play counts, ratings and playlist
//! memberships; the preview lists them.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::{
    engine::execution::device,
    engine::planning::source_collection::SelectionOptions,
    engine::state::journal::{CoordinationId, CoordinationKind, Identity, Journal},
    engine::state::layout,
    engine::state::snapshot,
    engine::verification::audio_capture,
    engine::{
        Engine, Event, PlanRequest, SyncOutcome,
        execution::{removal_batch, sync},
        planning::change::MAX_BATCH,
    },
};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;

/// A coordination begins in phase 0, removing the selected tracks. Phase 1:
/// none of them is on the phone any more; adding the sources.
const ADDING: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Remove every track, then add the whole selection again.
    Library,
    /// Remove the managed tracks whose source audio changed, or whose source
    /// cleared a field an edit cannot clear, then add their sources again.
    Changed,
}
impl Kind {
    fn coordination(self) -> CoordinationKind {
        match self {
            Self::Library => CoordinationKind::ReplaceLibrary,
            Self::Changed => CoordinationKind::ReplaceTrack,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Changed => "changed",
        }
    }
}

pub struct Request<'a> {
    pub kind: Kind,
    pub source: &'a Path,
    pub state: &'a Path,
    pub journal: Option<&'a Path>,
    pub selection: &'a SelectionOptions,
    pub host_name: &'a str,
    pub seconds: u64,
    /// New tracks per insert session in the adding phase.
    pub batch_size: usize,
    /// Show the proposal and stop; nothing changes.
    pub dry_run: bool,
    /// `--flac-to-alac`: the adding sync sends each FLAC as an ALAC.
    pub flac_to_alac: bool,
    /// A saved plan's actions (`apply`): the tracks this run removes, and the
    /// sources its adding sync adds, must be exactly the plan's.
    pub pinned: Option<&'a crate::engine::ChangePins>,
}

/// One phone track the replacement removes.
#[derive(Clone, Debug, Serialize)]
pub struct Removal {
    pub track: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub plays: u32,
    /// The source that replaces it (changed audio only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// What the user confirms.
pub struct Preview<'a> {
    pub kind: Kind,
    /// An Active coordination this run continues; nothing new is confirmed.
    pub continuing: Option<i64>,
    pub removals: &'a [Removal],
    pub sources: usize,
    pub selection: &'a Path,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to replace (changed audio only).
    Nothing,
    Preview,
    Cancelled,
    Completed {
        coordination: i64,
        removed: usize,
        tracks: usize,
    },
}

/// The coordination's exact intent; continuing requires the same source and
/// selection, except that a whole-library replacement may exclude more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Intent {
    schema: String,
    kind: String,
    source: PathBuf,
    recursive: bool,
    include: BTreeSet<String>,
    exclude: BTreeSet<String>,
    /// Changed audio only: the tracks removed and the sources that replace them.
    tracks: Vec<String>,
    sources: Vec<String>,
}
impl Intent {
    /// The arguments that continue this replacement: `sync` replaces the
    /// library directory, so the directory it began with is set first.
    /// `connection` ends the `sync` (`Engine::command_options`).
    fn arguments(&self, connection: &str) -> String {
        format!(
            "config library {} && rustunes sync {}{connection}",
            quoted(&self.source.to_string_lossy()),
            match self.kind.as_str() {
                "library" => "--replace-library",
                _ => "--replace-changed",
            }
        )
    }
}

/// The `sync` arguments that continue an Active replacement coordination,
/// from its recorded intent, with the `connection` options of the command
/// that prints them (`Engine::command_options`, or "" for the bare
/// arguments); `None` for another kind or an unreadable intent.
pub(crate) fn continuation(
    kind: CoordinationKind,
    intent: &[u8],
    connection: &str,
) -> Option<String> {
    matches!(
        kind,
        CoordinationKind::ReplaceLibrary | CoordinationKind::ReplaceTrack
    )
    .then(|| serde_json::from_slice::<Intent>(intent).ok())
    .flatten()
    .map(|intent| intent.arguments(connection))
}

/// A word the shell reads back unchanged.
pub(crate) fn quoted(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=,:@%".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

pub(crate) async fn run(
    r: Request<'_>,
    engine: &Engine,
    confirm: &mut impl FnMut(Preview<'_>) -> Result<bool>,
) -> Result<Outcome> {
    engine.check()?;
    ensure!(
        (1..=MAX_BATCH).contains(&r.batch_size),
        "batch size must be 1..{MAX_BATCH}"
    );
    let source = std::fs::canonicalize(r.source).context("opening the source directory")?;
    let selected = crate::engine::planning::source_collection::select(&source, r.selection)?;
    if r.kind == Kind::Library {
        insertable(engine, &selected, r.flac_to_alac)?;
    }
    let run = layout::new_run(r.state)?;
    engine.emit(Event::RunEvidence(&run))?;
    let saved = capture(engine, &run.join("snapshot"), r.seconds).await?;
    let identity = Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &identity::parse(&saved.raw_preferences)?,
    )?;
    let journal_path = match r.journal {
        Some(path) => std::path::absolute(path)?,
        None => {
            crate::engine::execution::pull::current_journal(
                r.state,
                &saved,
                Some(&source),
                r.selection.recursive,
                &run,
                &engine.control(r.seconds),
            )
            .await?
        }
    };
    let active = if journal_path.join("journal.sqlite").try_exists()? {
        let journal = Journal::open_read_only(&journal_path, &identity)?;
        journal.active_coordination()?
    } else {
        None
    };
    let wanted = Intent {
        schema: "rustunes.replacement-intent".into(),
        kind: r.kind.as_str().into(),
        source: source.clone(),
        recursive: r.selection.recursive,
        include: r.selection.include.clone(),
        exclude: r.selection.exclude.clone(),
        tracks: Vec::new(),
        sources: Vec::new(),
    };
    let (intent, continuing) = match &active {
        Some(active) => {
            ensure!(
                r.pinned.is_none(),
                "replacement {} became Active after the saved plan; nothing was sent; continue it with: rustunes {}",
                active.id.as_i64(),
                continuation(active.kind, &active.intent, &engine.command_options())
                    .unwrap_or_else(|| "the command that began it".into())
            );
            ensure!(
                active.kind == r.kind.coordination(),
                "coordination {} is Active; finish it first",
                active.id.as_i64()
            );
            let intent: Intent = serde_json::from_slice(&active.intent)
                .context("the Active replacement's intent is unreadable")?;
            let narrowed = r.kind == Kind::Library && wanted.exclude.is_superset(&intent.exclude);
            ensure!(
                Intent {
                    tracks: Vec::new(),
                    sources: Vec::new(),
                    exclude: if narrowed {
                        wanted.exclude.clone()
                    } else {
                        intent.exclude.clone()
                    },
                    ..intent.clone()
                } == wanted,
                "replacement {} is Active with another source or selection; continue it with: rustunes {}",
                active.id.as_i64(),
                intent.arguments(&engine.command_options())
            );
            (intent, Some(active.id))
        }
        None => match r.kind {
            Kind::Library => (wanted, None),
            Kind::Changed => {
                let changed = changed_audio(
                    engine,
                    &source,
                    &run,
                    &journal_path,
                    r.selection,
                    r.flac_to_alac,
                )
                .await?;
                if let Some(pinned) = r.pinned {
                    pinned.check_replacements(&changed)?;
                }
                if changed.is_empty() {
                    return Ok(Outcome::Nothing);
                }
                let (tracks, sources) = changed.into_iter().unzip();
                (
                    Intent {
                        tracks,
                        sources,
                        ..wanted
                    },
                    None,
                )
            }
        },
    };
    let targets = removal_targets(r.kind, &intent, &saved)?;
    if let Some(pinned) = r.pinned {
        pinned.check_removals(&targets.iter().copied().collect())?;
    }
    let removals: Vec<Removal> = targets
        .iter()
        .map(|id| {
            let track = saved
                .catalog
                .tracks()
                .iter()
                .find(|t| t.sync_id == *id)
                .expect("removal targets come from this catalogue");
            Removal {
                track: format!("{id:016x}"),
                title: track.metadata.title.clone(),
                artist: track.metadata.artist.clone(),
                album: track.metadata.album.clone(),
                plays: track.playback.play_count_user,
                source: intent
                    .tracks
                    .iter()
                    .position(|t| *t == format!("{id:016x}"))
                    .map(|n| intent.sources[n].clone()),
            }
        })
        .collect();
    let selection_file = run.join("replacement.json");
    private_write(
        &selection_file,
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.replacement-proposal","intent":intent,
            "continuing":continuing.map(|c| c.as_i64()),"removals":removals,
            "sources":selected.files().len(),
            "losses":"removed tracks lose their identities, play and skip counts, ratings and playlist memberships",
        }))?,
    )?;
    let preview = Preview {
        kind: r.kind,
        continuing: continuing.map(|c| c.as_i64()),
        removals: &removals,
        sources: selected.files().len(),
        selection: &selection_file,
    };
    if r.dry_run {
        confirm(preview)?;
        return Ok(Outcome::Preview);
    }
    if !confirm(preview)? {
        return Ok(Outcome::Cancelled);
    }
    let id = match continuing {
        Some(id) => id,
        None => {
            if !journal_path.join("journal.sqlite").try_exists()? {
                drop(Journal::create(&journal_path, &identity)?);
            }
            let mut journal = Journal::open(&journal_path, &identity)?;
            journal.begin_coordination(
                r.kind.coordination(),
                &serde_json::to_vec(&intent)?,
                &run,
                saved.catalog.persisted_media_anchor(),
            )?
        }
    };
    // Ordinary journal writes of this process (the sync's single inserts and
    // covers) pass while the permit is held; every other process is refused.
    let permit = Journal::open(&journal_path, &identity)?.enter_coordination(id)?;
    cancel_unstarted(&journal_path, &identity)?;
    ensure_settled(
        &journal_path,
        &identity,
        r.state,
        id,
        &engine.command_options(),
    )?;
    let removed = if phase(&journal_path, &identity, id)? < ADDING {
        let removed = remove(&r, engine, &journal_path, &identity, id, &run, &targets).await?;
        let after = capture(engine, &run.join("removed-snapshot"), r.seconds).await?;
        let left = removal_targets(r.kind, &intent, &after)?;
        ensure!(
            left.is_empty()
                && (r.kind == Kind::Changed
                    || after.index.tracks().is_empty() && after.index.other_audio().is_empty()),
            "{} selected tracks are still on the phone; replacement {} stays Active and running it again continues",
            left.len(),
            id.as_i64()
        );
        let mut journal = Journal::open(&journal_path, &identity)?;
        let revision = journal.coordination(id)?.revision;
        journal.advance_coordination(id, revision, ADDING)?;
        removed
    } else {
        0
    };
    engine.emit(Event::ReplacementPhase {
        coordination: id.as_i64(),
        phase: "adding",
    })?;
    let selection = match r.kind {
        Kind::Library => r.selection.clone(),
        Kind::Changed => SelectionOptions {
            include: intent.sources.iter().cloned().collect(),
            ..r.selection.clone()
        },
    };
    let result = Box::pin(sync::run_with(
        sync::Options {
            source: &source,
            state: r.state,
            snapshot: None,
            journal: Some(&journal_path),
            selection: &selection,
            host_name: r.host_name,
            dry_run: false,
            seconds: r.seconds,
            add_only: true,
            batch_size: r.batch_size,
            flac_to_alac: r.flac_to_alac,
            pinned: r.pinned,
            library: None,
            removals: &[],
        },
        Some(id),
        engine,
    ))
    .await
    .with_context(|| {
        format!(
            "replacement {} stays Active; run the same command again to continue",
            id.as_i64()
        )
    })?;
    ensure!(
        result.outcome == SyncOutcome::Completed,
        "the adding sync was blocked ({}); replacement {} stays Active",
        result.plan.display(),
        id.as_i64()
    );
    // A source the adding sync left out would stay off the phone.
    ensure!(
        result.skipped.is_empty(),
        "the adding sync left out {}; fix them and run the same command again, as replacement {} stays Active:\n{}",
        result.skipped.len(),
        id.as_i64(),
        result
            .skipped
            .iter()
            .map(|s| format!("  {}: {}", s.path, s.reason))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Complete only when a fresh add-only plan has nothing left to add.
    let finished = capture(engine, &run.join("final-snapshot"), r.seconds).await?;
    let plan = run.join("final-plan.json");
    let report = {
        let engine = engine.clone();
        let (source, snapshot, journal, plan) = (
            source.clone(),
            run.join("final-snapshot"),
            journal_path.clone(),
            plan.clone(),
        );
        let selection = selection.clone();
        let flac_to_alac = r.flac_to_alac;
        tokio::task::spawn_blocking(move || {
            engine.plan(PlanRequest {
                source: &source,
                snapshot: &snapshot,
                journal: Some(&journal),
                output: Some(&plan),
                selection: &selection,
                add_only: true,
                flac_to_alac,
            })
        })
        .await
        .context("final planning task failed")??
    };
    ensure!(
        report.additions == 0 && report.skipped == 0 && report.ready(),
        "{} sources are still missing on the phone ({}); replacement {} stays Active",
        report.additions + report.skipped,
        plan.display(),
        id.as_i64()
    );
    let pending_covers = report
        .plan
        .sources
        .iter()
        .filter(|row| {
            matches!(
                row.artwork,
                crate::engine::planning::additive::ArtworkDecision::PendingOwnedCover { .. }
            )
        })
        .count();
    ensure!(
        pending_covers == 0,
        "{pending_covers} covers of added tracks are still pending ({}); replacement {} stays Active",
        plan.display(),
        id.as_i64()
    );
    let tracks = finished.catalog.tracks().len();
    let outcome = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.replacement-outcome","kind":r.kind.as_str(),
        "excluded_since_start":selection.exclude.difference(&intent.exclude).collect::<Vec<_>>(),
        "removed_this_run":removed,"tracks":tracks,"matched":report.matched,
        "anchor":finished.catalog.persisted_media_anchor(),
        "final_plan":plan,
    }))?;
    private_write(&run.join("replacement-outcome.json"), &outcome)?;
    let mut journal = Journal::open(&journal_path, &identity)?;
    let revision = journal.coordination(id)?.revision;
    journal.complete_coordination(id, revision, &outcome)?;
    drop(permit);
    Ok(Outcome::Completed {
        coordination: id.as_i64(),
        removed,
        tracks,
    })
}

/// Refuse a whole-library replacement while a selected source is one this
/// tool can never add (with `flac_to_alac`, a FLAC it cannot convert); each
/// is named so it can be excluded.
fn insertable(
    engine: &Engine,
    selected: &crate::engine::planning::source_collection::Selection,
    flac_to_alac: bool,
) -> Result<()> {
    let mut last = std::time::Instant::now();
    let mut sources = crate::engine::planning::source_collection::inspect_with_progress(
        selected,
        |done, total| {
            engine.check()?;
            if done == 0 || done == total || last.elapsed().as_secs() >= 5 {
                engine.emit(Event::InspectingSources { done, total })?;
                last = std::time::Instant::now();
            }
            Ok(())
        },
    )?;
    sources.flac_to_alac = flac_to_alac;
    let refused = crate::engine::planning::additive::uninsertable_sources(&sources);
    ensure!(
        refused.is_empty(),
        "{} selected source(s) cannot be added by this tool, so replacing the library would leave them off the phone; exclude each with --exclude PATH or make it supported first:\n{}",
        refused.len(),
        refused
            .iter()
            .map(|(path, reason)| format!("  {path}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}

/// A fresh complete snapshot under `path`.
async fn capture(engine: &Engine, path: &Path, seconds: u64) -> Result<snapshot::Snapshot> {
    engine.emit(Event::ReadingDeviceSnapshot)?;
    let control = engine.control(seconds);
    control.check()?;
    Box::pin(audio_capture::capture_with_control(
        path,
        &control,
        device::provider,
    ))
    .await?;
    snapshot::open(path)
}

/// A run stopped between preparing and starting one of its single inserts or
/// covers leaves that attempt Prepared, and nothing was sent. Only this
/// coordination can have prepared it (a coordination begins on a quiescent
/// journal and owns it from then on), so continuing cancels it, as
/// `resume --cancel-prepared` does outside a coordination. Started work is
/// left to `resume`, which observes what reached the phone.
pub(crate) fn cancel_unstarted(journal: &Path, identity: &Identity) -> Result<()> {
    let mut journal = Journal::open(journal, identity)?;
    if let Some(attempt) = journal.outstanding_attempt()?
        && attempt.state == crate::engine::state::journal::AttemptState::Prepared
    {
        journal.cancel_prepared(attempt.id)?;
    }
    Ok(())
}

/// Refuse to go on past an attempt this coordination started and did not
/// finish: only `resume`, observing the phone, settles it. The refusal names
/// the command, with the run that holds the attempt's input when found.
fn ensure_settled(
    journal: &Path,
    identity: &Identity,
    state: &Path,
    id: CoordinationId,
    connection: &str,
) -> Result<()> {
    let Some(attempt) = Journal::open_read_only(journal, identity)?.outstanding_attempt()? else {
        return Ok(());
    };
    anyhow::bail!(
        "attempt {} was started and not finished; settle it with {}, then run the same command again; replacement {} stays Active",
        attempt.id.as_i64(),
        resume_command(journal, state, &attempt, connection),
        id.as_i64()
    )
}

/// Refuse a run outside a coordination while a single insert or cover is
/// unfinished, naming the `resume` that settles it: a Prepared attempt sent
/// nothing and is cancelled (with `snapshot`, this run's), a Started one is
/// observed on the phone. Without this the plan only reports every source as
/// needing reconciliation. Each `resume` ends with `connection`
/// (`Engine::command_options`).
pub(crate) fn ensure_no_unfinished_attempt(
    journal: &Path,
    identity: &Identity,
    state: &Path,
    snapshot: &Path,
    connection: &str,
) -> Result<()> {
    let opened = Journal::open_read_only(journal, identity)?;
    if opened.active_coordination()?.is_some() {
        return Ok(());
    }
    // A batch is settled from the journal, whose record names its evidence;
    // any snapshot of this phone identifies it.
    let batch = if let Some(b) = opened.outstanding_removal_batch()? {
        Some(session_or_removal(&b))
    } else if let Some(b) = opened.outstanding_insert_batch()? {
        Some(format!("insert batch {}", b.id.as_i64()))
    } else {
        opened
            .outstanding_metadata_batch()?
            .map(|b| format!("metadata batch {}", b.id.as_i64()))
    };
    if let Some(batch) = batch {
        anyhow::bail!(
            "{batch} is unfinished; settle it with rustunes resume --snapshot {} --journal {} --trace-dir NEW_DIRECTORY{connection}, then run this command again; resume observes the phone and never resends",
            quoted(&snapshot.to_string_lossy()),
            quoted(&journal.to_string_lossy()),
        );
    }
    let Some(attempt) = opened.outstanding_attempt()? else {
        return Ok(());
    };
    let id = attempt.id.as_i64();
    if attempt.state == crate::engine::state::journal::AttemptState::Prepared {
        anyhow::bail!(
            "attempt {id} was prepared and never started, so nothing was sent; cancel it with rustunes resume --snapshot {} --journal {} --trace-dir NEW_DIRECTORY --cancel-prepared {id}{connection}, then run this command again",
            quoted(&snapshot.to_string_lossy()),
            quoted(&journal.to_string_lossy()),
        );
    }
    anyhow::bail!(
        "attempt {id} was started and not finished; settle it with {}, then run this command again; resume observes the phone and never resends",
        resume_command(journal, state, &attempt, connection)
    )
}

/// An unfinished removal batch by name: a session's removal half names its
/// session.
pub(crate) fn session_or_removal(batch: &crate::engine::state::journal::BatchRecord) -> String {
    match batch.with_insert_batch {
        Some(insert) => format!(
            "the session of removal batch {} and insert batch {}",
            batch.id.as_i64(),
            insert.as_i64()
        ),
        None => format!("removal batch {}", batch.id.as_i64()),
    }
}

/// The `resume` that settles `attempt`, with the run holding its input. A run
/// outside this host's runs (a cover `rustunes artwork` traced where it was
/// told) is named by the options that placed it.
fn resume_command(
    journal: &Path,
    state: &Path,
    attempt: &crate::engine::state::journal::Attempt,
    connection: &str,
) -> String {
    let journal = quoted(&journal.to_string_lossy());
    match run_holding(&state.join("runs"), attempt.prepared.input_receipt_sha256) {
        Some(run) => format!(
            "rustunes resume --snapshot {} --journal {journal} --run-dir {} --trace-dir NEW_DIRECTORY{connection}",
            quoted(&run.join("snapshot").to_string_lossy()),
            quoted(&run.to_string_lossy())
        ),
        None => format!(
            "rustunes resume --snapshot SNAPSHOT --journal {journal} --run-dir RUN --trace-dir NEW_DIRECTORY{connection}, where {}",
            match attempt.prepared.kind {
                crate::engine::state::journal::AttemptKind::Artwork => {
                    "RUN is the --trace-dir and SNAPSHOT the --snapshot of the rustunes artwork that started it"
                }
                crate::engine::state::journal::AttemptKind::Insert => {
                    "RUN is the run that started it and SNAPSHOT that run's snapshot"
                }
            }
        ),
    }
}

/// The run directory holding an `input.json` (in the run itself, as
/// `cover-0001/` or as `audio/step-0001/`) whose SHA-256 is `digest`.
fn run_holding(runs: &Path, digest: [u8; 32]) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    fn holds(directory: &Path, digest: &[u8; 32], depth: usize) -> bool {
        let input = directory.join("input.json");
        if std::fs::read(&input).is_ok_and(|bytes| Sha256::digest(bytes).as_slice() == digest) {
            return true;
        }
        depth > 0
            && std::fs::read_dir(directory).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry.file_type().is_ok_and(|t| t.is_dir())
                        && holds(&entry.path(), digest, depth - 1)
                })
            })
    }
    std::fs::read_dir(runs)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|run| holds(run, &digest, 2))
}

fn phase(journal: &Path, identity: &Identity, id: CoordinationId) -> Result<u8> {
    Ok(Journal::open_read_only(journal, identity)?
        .coordination(id)?
        .phase)
}

/// The tracks still to remove in `saved`: every track for a library
/// replacement, the intent's tracks still present for changed audio.
fn removal_targets(kind: Kind, intent: &Intent, saved: &snapshot::Snapshot) -> Result<Vec<u64>> {
    let present = saved.catalog.tracks().iter().map(|t| t.sync_id);
    Ok(match kind {
        Kind::Library => present.collect(),
        Kind::Changed => {
            let wanted = intent
                .tracks
                .iter()
                .map(|t| u64::from_str_radix(t, 16).context("replacement track id"))
                .collect::<Result<BTreeSet<_>>>()?;
            present.filter(|id| wanted.contains(id)).collect()
        }
    })
}

/// Remove `targets` in verified batches linked to the coordination.
async fn remove(
    r: &Request<'_>,
    engine: &Engine,
    journal: &Path,
    identity: &Identity,
    id: CoordinationId,
    run: &Path,
    targets: &[u64],
) -> Result<usize> {
    engine.emit(Event::ReplacementPhase {
        coordination: id.as_i64(),
        phase: "removing",
    })?;
    if targets.is_empty() {
        return Ok(0);
    }
    let directory = run.join("remove");
    crate::engine::execution::native_insert::create_trace(&directory)?;
    let settled = removal_batch::remove_all(
        journal,
        identity,
        targets,
        r.batch_size,
        Some(id),
        &directory,
        r.host_name,
        || engine.control(r.seconds),
        |_, removed, total| engine.emit(Event::ReplacementRemoved { removed, total }),
    )
    .await
    .with_context(|| {
        format!(
            "replacement {} stays Active; run resume if a batch is unfinished, then the same command again",
            id.as_i64()
        )
    })?;
    Ok(settled.iter().map(|b| b.removed).sum())
}

/// The managed tracks only a replacement brings up to date, as (track id,
/// source relative path), from a fresh full plan: those whose source audio
/// changed at their owned path, and those whose source clears a field an edit
/// cannot clear (`metadata_update_needs_replacement`).
async fn changed_audio(
    engine: &Engine,
    source: &Path,
    run: &Path,
    journal: &Path,
    selection: &SelectionOptions,
    flac_to_alac: bool,
) -> Result<Vec<(String, String)>> {
    ensure!(
        journal.join("journal.sqlite").try_exists()?,
        "changed audio is only replaced for tracks this tool inserted; there is no journal"
    );
    let engine = engine.clone();
    let (source, snapshot, journal, plan) = (
        source.to_path_buf(),
        run.join("snapshot"),
        journal.to_path_buf(),
        run.join("plan.json"),
    );
    let selection = selection.clone();
    let report = tokio::task::spawn_blocking(move || {
        engine.plan(PlanRequest {
            source: &source,
            snapshot: &snapshot,
            journal: Some(&journal),
            output: Some(&plan),
            selection: &selection,
            add_only: false,
            flac_to_alac,
        })
    })
    .await
    .context("planning task failed")??;
    replacements(&report.plan)
}

/// The managed tracks only a replacement brings up to date in a full plan
/// (not add-only), as (track id, source relative path): those whose source
/// audio changed at their owned path, and those whose source clears a field
/// an edit cannot clear (`metadata_update_needs_replacement`).
pub(crate) fn replacements(
    plan: &crate::engine::planning::additive::Plan,
) -> Result<Vec<(String, String)>> {
    plan.sources
        .iter()
        .filter_map(|row| {
            if let crate::engine::planning::additive::Decision::Skip {
                code: "metadata_update_needs_replacement",
                ..
            } = row.decision
            {
                return Some(match &row.matches[..] {
                    [owned] if owned.ownership == "inserted_by_us" => {
                        Ok((owned.track_id.clone(), row.relative_path.clone()))
                    }
                    _ => Err(anyhow::anyhow!(
                        "{} names no single track this tool inserted",
                        row.relative_path
                    )),
                });
            }
            let replacement = row
                .managed_audio_replacement
                .as_ref()
                .filter(|d| d.replacement_identity_established)?;
            Some(
                replacement
                    .evidence
                    .as_ref()
                    .and_then(|e| e["immutable_original_content"]["track_id"].as_str())
                    .map(|id| (id.to_owned(), row.relative_path.clone()))
                    .context("replacement evidence names no original track"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(kind: &str) -> Intent {
        Intent {
            schema: "rustunes.replacement-intent".into(),
            kind: kind.into(),
            source: "/home/u/Music/ALAC".into(),
            recursive: true,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
            tracks: Vec::new(),
            sources: Vec::new(),
        }
    }

    #[test]
    fn continuation_sets_the_library_it_began_with_and_syncs() {
        let mut changed = intent("changed");
        changed.include = ["Selective Tone A.m4a".into(), "sub".into()].into();
        changed.exclude = ["it's.m4a".into()].into();
        changed.recursive = false;
        changed.tracks = vec!["00000000000003e9".into()];
        changed.sources = vec!["Selective Tone A.m4a".into()];
        let bytes = serde_json::to_vec(&changed).unwrap();
        assert_eq!(
            continuation(CoordinationKind::ReplaceTrack, &bytes, "").unwrap(),
            "config library /home/u/Music/ALAC && rustunes sync --replace-changed"
        );
        let mut library = intent("library");
        library.exclude = ["Artist - Title.mp3".into()].into();
        let bytes = serde_json::to_vec(&library).unwrap();
        assert_eq!(
            continuation(CoordinationKind::ReplaceLibrary, &bytes, "").unwrap(),
            "config library /home/u/Music/ALAC && rustunes sync --replace-library"
        );
        // A command printed over Wi-Fi keeps the phone reachable the same way.
        assert_eq!(
            continuation(
                CoordinationKind::ReplaceLibrary,
                &bytes,
                " --wifi-address 192.0.2.7"
            )
            .unwrap(),
            "config library /home/u/Music/ALAC && rustunes sync --replace-library --wifi-address 192.0.2.7"
        );
        assert!(continuation(CoordinationKind::Clear, &bytes, "").is_none());
        assert!(continuation(CoordinationKind::ReplaceTrack, b"intent", "").is_none());
    }

    #[test]
    fn the_run_holding_an_attempt_input_is_found_by_its_digest() {
        use sha2::{Digest, Sha256};
        let runs = std::env::temp_dir().join(format!(
            "replace-runs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for (dir, input) in [
            ("1/audio/step-0001", "insert"),
            ("2/cover-0001", "cover"),
            ("3/audio/step-0001/deeper", "too deep"),
        ] {
            std::fs::create_dir_all(runs.join(dir)).unwrap();
            std::fs::write(runs.join(dir).join("input.json"), input).unwrap();
        }
        let digest = |bytes: &str| -> [u8; 32] { Sha256::digest(bytes).into() };
        assert_eq!(run_holding(&runs, digest("insert")), Some(runs.join("1")));
        assert_eq!(run_holding(&runs, digest("cover")), Some(runs.join("2")));
        assert_eq!(run_holding(&runs, digest("too deep")), None);
        assert_eq!(run_holding(&runs.join("missing"), digest("insert")), None);
        std::fs::remove_dir_all(&runs).unwrap();
    }

    #[test]
    fn an_unfinished_single_attempt_refuses_with_the_resume_that_settles_it() {
        use crate::engine::state::journal::test_support;
        let temp = test_support::Temp::new();
        let identity = test_support::identity();
        let mut journal = Journal::create(&temp.path(), &identity).unwrap();
        let (state, snapshot) = (temp.0.join("state"), temp.0.join("snapshot"));
        ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, "").unwrap();
        let id = test_support::prepare_insert(&mut journal, test_support::alac_content(1));
        let error = ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, "")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("never started")
                && error.contains(&format!("--cancel-prepared {}", id.as_i64()))
                && error.contains(&snapshot.to_string_lossy().to_string()),
            "{error}"
        );
        drop(
            journal
                .mark_started(
                    id,
                    crate::engine::state::journal::StartEvidence {
                        ready_media_anchor: 13,
                        ready_receipt_sha256: [8; 32],
                        signature: [1; 21],
                    },
                )
                .unwrap(),
        );
        let error = ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, "")
            .unwrap_err()
            .to_string();
        // No run under this state holds its input: the options are named.
        assert!(
            error.contains("started and not finished")
                && error.contains("--run-dir RUN --trace-dir NEW_DIRECTORY, where RUN is the run")
                && error.contains(&temp.path().to_string_lossy().to_string()),
            "{error}"
        );
        let mut cover = journal.outstanding_attempt().unwrap().unwrap();
        cover.prepared.kind = crate::engine::state::journal::AttemptKind::Artwork;
        assert!(
            resume_command(&temp.path(), &state, &cover, " --wifi").ends_with(
                "--trace-dir NEW_DIRECTORY --wifi, where RUN is the --trace-dir and SNAPSHOT the --snapshot of the rustunes artwork that started it"
            )
        );
        journal.mark_not_applied(id, [7; 32]).unwrap();
        ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, "").unwrap();
        // An unfinished batch names the resume that settles it.
        let batch = journal
            .prepare_removal_batch(&crate::engine::state::journal::RemovalIntent {
                coordination: None,
                compensates: None,
                compensates_insert_batch: None,
                operation: vec![5; 40],
                base_anchor: 13,
                before_current: [1; 32],
                before_index: [2; 32],
                input_receipt: [3; 32],
                evidence: "/run/removal-fixture".into(),
                targets: vec![crate::engine::state::journal::RemovalTarget {
                    track: 4242,
                    item_pid: 4242,
                    path: Some("iTunes_Control/Music/F40/U001.mp3".into()),
                    file_bytes: Some(900),
                    file_sha256: Some([55; 32]),
                    binding: None,
                }],
                playlists: Vec::new(),
            })
            .unwrap();
        let error =
            ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, " --wifi")
                .unwrap_err()
                .to_string();
        assert!(
            error.contains(&format!("removal batch {} is unfinished", batch.as_i64()))
                && error.contains("resume --snapshot")
                && error.contains("--trace-dir NEW_DIRECTORY --wifi,")
                && error.contains(&snapshot.to_string_lossy().to_string()),
            "{error}"
        );
        // A session is named by both halves.
        journal.cancel_removal_batch(batch).unwrap();
        let removal = journal.removal_batch(batch).unwrap();
        let mut insert = crate::engine::state::journal::InsertIntent {
            coordination: None,
            operation: vec![6; 40],
            base_anchor: 13,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/session-fixture".into(),
            targets: Vec::new(),
        };
        insert
            .targets
            .push(crate::engine::state::journal::InsertTarget {
                content: test_support::alac_content(2),
                new_album_pid: None,
                new_artist_pid: None,
                conversion: None,
                source: None,
            });
        let ids = journal
            .prepare_session(
                &insert,
                &crate::engine::state::journal::RemovalIntent {
                    coordination: None,
                    compensates: None,
                    compensates_insert_batch: None,
                    operation: vec![6; 40],
                    base_anchor: 13,
                    before_current: [1; 32],
                    before_index: [2; 32],
                    input_receipt: [3; 32],
                    evidence: "/run/session-fixture".into(),
                    targets: removal.targets,
                    playlists: Vec::new(),
                },
            )
            .unwrap();
        let error = ensure_no_unfinished_attempt(&temp.path(), &identity, &state, &snapshot, "")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "the session of removal batch {} and insert batch {} is unfinished",
                ids.removal.as_i64(),
                ids.insert.as_i64()
            )),
            "{error}"
        );
    }
}
