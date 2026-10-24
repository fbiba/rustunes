//! Saved plans for the syncs that remove tracks before adding: `--remove-missing`
//! and `--add-remove` remove the tracks this tool inserted whose source file is
//! gone, then sync; `--replace-changed` and `--replace-library` remove tracks
//! and add their sources again. A plan names every track it removes, why, and
//! the source that comes back in its place; the additions and updates of the
//! sync that follows; what the run costs; and everything that blocks it.
//!
//! The steps depend on each other. The removal goes first, and the sync after
//! it plans again from a fresh capture, since the removal changes the
//! catalogue; new tracks get their identities then, so the plan names none.
//! The plan is built from saved state only and grants nothing: `apply`
//! rebuilds it from the same snapshot, sources and journal and requires the
//! same bytes, then pins each step to it (`Pinned`). The removal must select
//! exactly its tracks and the sync after it must add and update exactly its
//! sources, each checked against fresh state before that step sends anything.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use super::saved_plan;
use crate::{
    engine::planning::additive::{self, ArtworkDecision, Decision, JournalEvidence},
    engine::planning::candidates,
    engine::planning::removal::{self, Origin},
    engine::planning::source_collection::{self, SelectionOptions},
    engine::state::current::hex,
    engine::state::journal::{CoordinationKind, Identity, Journal},
    engine::state::snapshot,
    engine::{
        Engine, Event,
        execution::{removal_batch, replace},
    },
};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;

pub const SCHEMA: &str = "rustunes.change-plan";
/// A session's operations by default, besides its `update_db_info`: all one
/// operation file holds, for every command that sends.
pub(crate) const DEFAULT_BATCH: usize = MAX_BATCH;
/// The largest `--batch-size`: a session's operations besides its
/// `update_db_info`, which one operation file holds.
pub(crate) const MAX_BATCH: usize = music_protocol::protocol::operations::MAX_OPERATIONS - 1;
const LOSSES: &str =
    "removed tracks lose their identities, play and skip counts, ratings and playlist memberships";
const DEPENDENCIES: &str = "the removal goes first; the sync after it plans again from a fresh capture and must add and update exactly these sources, and new tracks get their identities then";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Remove the tracks whose source file is gone, then sync.
    RemoveMissing,
    /// The same with an add-only sync (`--add-remove`).
    AddRemove,
    /// Remove the tracks whose source audio changed, or whose source cleared a
    /// field an edit cannot clear, then add their sources again.
    ReplaceChanged,
    /// Remove every track, then add the whole selection again.
    ReplaceLibrary,
}
impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RemoveMissing => "remove_missing",
            Self::AddRemove => "add_remove",
            Self::ReplaceChanged => "replace_changed",
            Self::ReplaceLibrary => "replace_library",
        }
    }
    fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "remove_missing" => Self::RemoveMissing,
            "add_remove" => Self::AddRemove,
            "replace_changed" => Self::ReplaceChanged,
            "replace_library" => Self::ReplaceLibrary,
            _ => anyhow::bail!("unknown saved plan action {text:?}"),
        })
    }
    /// Whether the sync after the removal keeps the audio and metadata of the
    /// tracks already on the phone.
    pub fn add_only(self) -> bool {
        !matches!(self, Self::RemoveMissing)
    }
    pub fn replaces(self) -> bool {
        matches!(self, Self::ReplaceChanged | Self::ReplaceLibrary)
    }
}

#[derive(Serialize)]
pub struct ChangePlan {
    pub schema: &'static str,
    pub action: &'static str,
    /// `--flac-to-alac`: the sync sends each new FLAC as an ALAC.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub flac_to_alac: bool,
    pub source_root: String,
    pub selection: Scope,
    pub device_sha256: String,
    pub library_sha256: String,
    pub database_sha256: String,
    pub media_anchor: u32,
    pub device_track_count: usize,
    pub removals: Vec<Removal>,
    /// Tracks this tool inserted whose source path the journal does not pin;
    /// they stay (`--remove-missing` only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_path_unknown: Vec<Value>,
    pub additions: Vec<Addition>,
    pub updates: Vec<Update>,
    /// Sources the sync leaves out without holding the rest
    /// (`additive::skip`), with why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// Covers of this tool's earlier additions the sync finishes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_covers: Vec<String>,
    /// Tracks on the phone the plan neither removes nor updates.
    pub untouched_tracks: usize,
    pub costs: Costs,
    pub order: [&'static str; 2],
    pub dependencies: &'static str,
    pub losses: &'static str,
    /// Empty when `apply` may run the plan.
    pub blockers: Vec<String>,
    pub fresh_state_required_before_execution: bool,
    pub execution_authorized_by_plan: bool,
}

#[derive(Serialize)]
pub struct Scope {
    pub recursive: bool,
    pub include: BTreeSet<String>,
    pub exclude: BTreeSet<String>,
    pub selected_files: usize,
}

#[derive(Serialize)]
pub struct Removal {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub plays: u32,
    pub skips: u32,
    pub file_bytes: Option<u64>,
    /// `source_gone`, `audio_changed` (or a cleared field) or `whole_library`.
    pub cause: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_source_path: Option<String>,
    /// The source (relative path) added again in its place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<String>,
    pub blockers: Vec<String>,
}

#[derive(Serialize)]
pub struct Addition {
    pub relative_path: String,
    pub file_bytes: Option<u64>,
    /// The bytes the sync sends.
    pub source_file_sha256: Option<String>,
    pub codec: &'static str,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub has_cover: bool,
    /// Why a source that holds pictures goes without a cover.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_left_out: Option<String>,
    /// The removed track it takes the place of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
}

#[derive(Serialize)]
pub struct Update {
    pub relative_path: String,
    /// The sync plan's decision for the row, exactly.
    pub decision: Value,
}

#[derive(Serialize)]
pub struct Skipped {
    pub relative_path: String,
    pub code: &'static str,
    /// Why, in plain words, and what to do.
    pub reason: String,
    /// The planner's full refusal.
    pub detail: String,
}

#[derive(Serialize)]
pub struct Costs {
    pub removal_sessions: usize,
    pub removal_batch_size: usize,
    pub upload_bytes: u64,
    pub plays_lost: u64,
    pub skips_lost: u64,
}

impl ChangePlan {
    pub fn ready(&self) -> bool {
        self.blockers.is_empty()
    }
    /// What `apply` holds each step to.
    pub fn pinned(&self) -> Result<Pinned> {
        let id = |text: &str| u64::from_str_radix(text, 16).context("planned track id");
        Ok(Pinned {
            removals: self
                .removals
                .iter()
                .map(|r| id(&r.track_id))
                .collect::<Result<_>>()?,
            replaced_by: self
                .removals
                .iter()
                .filter_map(|r| {
                    r.replaced_by
                        .as_ref()
                        .map(|s| Ok((id(&r.track_id)?, s.clone())))
                })
                .collect::<Result<_>>()?,
            additions: self
                .additions
                .iter()
                .map(|a| (a.relative_path.clone(), a.source_file_sha256.clone()))
                .collect(),
            updates: self
                .updates
                .iter()
                .map(|u| (u.relative_path.clone(), u.decision.clone()))
                .collect(),
        })
    }
}

/// The actions a saved plan holds each step to: the removal selects exactly
/// `removals` (for a changed-audio replacement, each replaced by its source),
/// and the sync after it adds exactly `additions` (each source path with the
/// SHA-256 of its file) and sends exactly `updates`, each with no conflict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pinned {
    pub removals: BTreeSet<u64>,
    pub replaced_by: BTreeMap<u64, String>,
    pub additions: BTreeMap<String, Option<String>>,
    pub updates: BTreeMap<String, Value>,
}

impl Pinned {
    /// What `apply` holds the sync of a saved additive plan to: its additions
    /// and updates, as a change plan's sync step is held to its own.
    pub(crate) fn from_additive(plan: &additive::Plan) -> Result<Self> {
        let mut pinned = Self::default();
        for row in &plan.sources {
            match &row.decision {
                Decision::Add { .. } => {
                    pinned
                        .additions
                        .insert(row.relative_path.clone(), row.source_file_sha256.clone());
                }
                Decision::Update { .. } => {
                    pinned.updates.insert(
                        row.relative_path.clone(),
                        serde_json::to_value(&row.decision)?,
                    );
                }
                _ => {}
            }
        }
        Ok(pinned)
    }
    /// The removal step's selection against fresh state, before it sends.
    pub(crate) fn check_removals(&self, fresh: &BTreeSet<u64>) -> Result<()> {
        ensure!(
            *fresh == self.removals,
            "the saved plan removes {} track(s) and the phone's current state selects {} ({}); nothing was sent; take a fresh snapshot and plan again",
            self.removals.len(),
            fresh.len(),
            differences(
                &self
                    .removals
                    .iter()
                    .map(|id| format!("{id:016x}"))
                    .collect(),
                &fresh.iter().map(|id| format!("{id:016x}")).collect()
            )
        );
        Ok(())
    }
    /// A changed-audio replacement's tracks and the sources that replace
    /// them, as (track id, source relative path), against fresh state.
    pub(crate) fn check_replacements(&self, fresh: &[(String, String)]) -> Result<()> {
        let planned: BTreeSet<String> = self
            .replaced_by
            .iter()
            .map(|(id, source)| format!("{id:016x} → {source}"))
            .collect();
        let fresh: BTreeSet<String> = fresh
            .iter()
            .map(|(id, source)| format!("{id} → {source}"))
            .collect();
        ensure!(
            planned == fresh,
            "the saved plan replaces {} track(s) and the phone's current state replaces {} ({}); nothing was sent; take a fresh snapshot and plan again",
            planned.len(),
            fresh.len(),
            differences(&planned, &fresh)
        );
        Ok(())
    }
    /// The sync after the removal, planned from fresh state, before it sends.
    pub(crate) fn check_sync(&self, plan: &additive::Plan) -> Result<()> {
        let addition = |path: &str, sha: &Option<String>| match sha {
            Some(sha) => format!("{path} (file {})", &sha[..sha.len().min(12)]),
            None => path.to_owned(),
        };
        let mut additions = BTreeSet::new();
        let mut updates = BTreeSet::new();
        let mut conflicts = Vec::new();
        for row in &plan.sources {
            match &row.decision {
                Decision::Add { .. } => {
                    additions.insert(addition(&row.relative_path, &row.source_file_sha256));
                }
                Decision::Update { .. } => {
                    updates.insert(format!(
                        "{} {}",
                        row.relative_path,
                        serde_json::to_value(&row.decision)?
                    ));
                }
                Decision::Conflict { code, .. } => {
                    conflicts.push(format!("{}: {code}", row.relative_path))
                }
                Decision::Keep | Decision::Skip { .. } => {}
            }
        }
        conflicts.extend(plan.journal_blockers.iter().cloned());
        let planned_additions: BTreeSet<String> = self
            .additions
            .iter()
            .map(|(path, sha)| addition(path, sha))
            .collect();
        let planned_updates: BTreeSet<String> = self
            .updates
            .iter()
            .map(|(path, decision)| format!("{path} {decision}"))
            .collect();
        let mut found = Vec::new();
        if additions != planned_additions {
            found.push(format!(
                "additions: {}",
                differences(&planned_additions, &additions)
            ));
        }
        if updates != planned_updates {
            found.push(format!(
                "updates: {}",
                differences(&planned_updates, &updates)
            ));
        }
        if !conflicts.is_empty() {
            found.push(format!("now blocked by {}", conflicts.join(", ")));
        }
        ensure!(
            found.is_empty(),
            "the sync no longer matches the saved plan ({}); it added and updated nothing",
            found.join("; ")
        );
        Ok(())
    }
}

/// Up to five entries on each side of two sets that differ.
fn differences(planned: &BTreeSet<String>, fresh: &BTreeSet<String>) -> String {
    let list = |set: Vec<&String>| {
        let mut text = set
            .iter()
            .take(5)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if set.len() > 5 {
            text.push_str(&format!(" and {} more", set.len() - 5));
        }
        text
    };
    let only_planned: Vec<_> = planned.difference(fresh).collect();
    let only_fresh: Vec<_> = fresh.difference(planned).collect();
    match (only_planned.is_empty(), only_fresh.is_empty()) {
        (true, true) => "the same".into(),
        (false, true) => format!("only in the plan: {}", list(only_planned)),
        (true, false) => format!("only now: {}", list(only_fresh)),
        (false, false) => format!(
            "only in the plan: {}; only now: {}",
            list(only_planned),
            list(only_fresh)
        ),
    }
}

pub struct Request<'a> {
    pub action: Action,
    pub source: &'a Path,
    pub snapshot: &'a Path,
    pub journal: Option<&'a Path>,
    pub output: Option<&'a Path>,
    pub selection: &'a SelectionOptions,
    pub flac_to_alac: bool,
}

pub struct Planned {
    pub plan: ChangePlan,
    pub serialized: Vec<u8>,
}

/// Plan a change offline from a complete snapshot; the bytes returned are the
/// bytes saved.
pub(crate) fn run(request: Request<'_>, engine: &Engine) -> Result<Planned> {
    engine.check()?;
    if let Some(output) = request.output {
        ensure!(
            !output.try_exists()?,
            "plan output already exists; choose a new file"
        );
    }
    engine.emit(Event::ReadingSavedSnapshot)?;
    let saved = snapshot::open(request.snapshot)?;
    let identity = Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &identity::parse(&saved.raw_preferences)?,
    )?;
    let journal = request
        .journal
        .map(|path| Journal::open_read_only(path, &identity))
        .transpose()?;
    let source = std::fs::canonicalize(request.source).context("opening the source")?;
    let plan = build(
        request.action,
        &source,
        request.selection,
        request.flac_to_alac,
        &saved,
        journal.as_ref().map(|journal| JournalEvidence {
            journal,
            identity: &identity,
        }),
        engine,
    )?;
    let mut serialized = serde_json::to_vec_pretty(&plan)?;
    serialized.push(b'\n');
    engine.check()?;
    if let Some(path) = request.output {
        private_write(path, &serialized)?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
        engine.emit(Event::PlanSaved(path))?;
    }
    Ok(Planned { plan, serialized })
}

/// Whether the file at `path` is a saved change plan (not an additive one).
pub fn is_change_plan(path: &Path) -> Result<bool> {
    let saved = saved_plan::unique_value(&saved_plan::read_stable(path)?)
        .context("reading saved JSON plan with unique object keys")?;
    Ok(saved["schema"] == SCHEMA)
}

/// A saved change plan rebuilt exactly from its snapshot, sources and journal.
pub struct Loaded {
    pub plan: ChangePlan,
    pub action: Action,
    pub source: PathBuf,
    pub selection: SelectionOptions,
    pub flac_to_alac: bool,
    pub plan_sha256: [u8; 32],
    pub pinned: Pinned,
}

/// Rebuild the saved plan at `plan_path` and require the same bytes.
pub(crate) fn load(
    plan_path: &Path,
    snapshot_path: &Path,
    journal_path: &Path,
    engine: &Engine,
) -> Result<Loaded> {
    use sha2::Digest;
    engine.check()?;
    let bytes = saved_plan::read_stable(plan_path)?;
    let saved = saved_plan::unique_value(&bytes)
        .context("reading saved JSON plan with unique object keys")?;
    ensure!(
        saved["schema"] == SCHEMA && saved["execution_authorized_by_plan"] == false,
        "unsupported saved plan schema"
    );
    let action = Action::parse(saved["action"].as_str().context("missing plan action")?)?;
    let source = PathBuf::from(
        saved["source_root"]
            .as_str()
            .context("missing source root")?,
    );
    ensure!(source.is_absolute(), "saved source root must be absolute");
    let scope = &saved["selection"];
    let selection = SelectionOptions {
        recursive: scope["recursive"]
            .as_bool()
            .context("missing recursive selection")?,
        include: selectors(&scope["include"])?,
        exclude: selectors(&scope["exclude"])?,
    };
    let flac_to_alac = match saved.get("flac_to_alac") {
        None => false,
        Some(value) => value.as_bool().context("invalid FLAC conversion policy")?,
    };
    engine.emit(Event::ReadingSavedSnapshot)?;
    let saved_snapshot = snapshot::open(snapshot_path)?;
    let identity = Identity::for_saved_device(
        saved_snapshot.identity.device_identifier_sha256,
        &identity::parse(&saved_snapshot.raw_preferences)?,
    )?;
    let journal = journal_path
        .join("journal.sqlite")
        .try_exists()?
        .then(|| Journal::open_read_only(journal_path, &identity))
        .transpose()?;
    let plan = build(
        action,
        &source,
        &selection,
        flac_to_alac,
        &saved_snapshot,
        journal.as_ref().map(|journal| JournalEvidence {
            journal,
            identity: &identity,
        }),
        engine,
    )?;
    ensure!(
        serde_json::to_value(&plan)? == saved,
        "saved plan differs from current sources, snapshot, journal or planner; generate a new plan"
    );
    let pinned = plan.pinned()?;
    Ok(Loaded {
        plan,
        action,
        source,
        selection,
        flac_to_alac,
        plan_sha256: sha2::Sha256::digest(&bytes).into(),
        pinned,
    })
}

fn selectors(value: &Value) -> Result<BTreeSet<String>> {
    let values = value.as_array().context("missing source selectors")?;
    ensure!(values.len() <= 4096, "source selector bound exceeded");
    let mut result = BTreeSet::new();
    for value in values {
        ensure!(
            result.insert(value.as_str().context("selector must be text")?.to_owned()),
            "duplicate source selector"
        );
    }
    Ok(result)
}

/// The plan for `action` on `root` and `options` against `saved`. Every input
/// is saved state or the source files; nothing here contacts the phone.
pub(crate) fn build(
    action: Action,
    root: &Path,
    options: &SelectionOptions,
    flac_to_alac: bool,
    saved: &snapshot::Snapshot,
    journal: Option<JournalEvidence<'_>>,
    engine: &Engine,
) -> Result<ChangePlan> {
    // Unfinished work leaves no state to plan from; name it and how it ends.
    if let Some(journal) = journal {
        let journal = journal.journal;
        let unfinished = [
            journal
                .outstanding_removal_batch()?
                .map(|b| crate::engine::execution::replace::session_or_removal(&b)),
            journal
                .outstanding_insert_batch()?
                .map(|b| format!("insert batch {}", b.id.as_i64())),
            journal
                .outstanding_metadata_batch()?
                .map(|b| format!("metadata batch {}", b.id.as_i64())),
        ];
        if let Some(batch) = unfinished.into_iter().flatten().next() {
            anyhow::bail!(
                "{batch} is unfinished; settle it with resume, or by running the command that started it again, then take a fresh snapshot and plan again"
            );
        }
    }
    let selected = source_collection::select(root, options)?;
    if matches!(action, Action::RemoveMissing | Action::AddRemove) {
        ensure!(
            selected.root() == selected.base_directory(),
            "--remove-missing needs the synced directory, not a file"
        );
    }
    let mut last = Instant::now();
    let mut sources = source_collection::inspect_with_progress(&selected, |done, total| {
        engine.check()?;
        if done == 0 || done == total || last.elapsed().as_secs() >= 5 {
            engine.emit(Event::InspectingSources { done, total })?;
            last = Instant::now();
        }
        Ok(())
    })?;
    sources.flac_to_alac = flac_to_alac;
    let candidates = candidates::generate(&sources, &saved.catalog, journal.map(|j| j.journal))?;
    engine.emit(Event::ComparingSources)?;
    let sync = additive::build_from_snapshot(
        &sources,
        saved,
        journal,
        &candidates,
        // A changed-audio replacement is found by the full comparison, and a
        // whole-library one matches each source to the track it replaces.
        matches!(action, Action::AddRemove | Action::ReplaceLibrary),
    )?;
    let mut blockers: Vec<String> = sync
        .journal_blockers
        .iter()
        .map(|b| format!("journal: {b}"))
        .collect();
    if let Some(journal) = journal
        && let Some(active) = journal.journal.active_coordination()?
    {
        let connection = engine.command_options();
        let command = match active.kind {
            CoordinationKind::Clear => Some(format!("clear{connection}")),
            kind => replace::continuation(kind, &active.intent, &connection),
        };
        blockers.push(format!(
            "coordination {} is Active; finish it first with: rustunes {}",
            active.id.as_i64(),
            command.unwrap_or_else(|| "the command that began it".into())
        ));
    }
    let row = |path: &str| sync.sources.iter().find(|r| r.relative_path == path);
    // The removal: each track, why, and the source that replaces it.
    let mut removed: BTreeMap<u64, (&'static str, Option<String>, Option<String>)> =
        BTreeMap::new();
    let mut source_path_unknown = Vec::new();
    // Sources a whole-library replacement could not add again.
    let mut refused = BTreeSet::new();
    match action {
        Action::RemoveMissing | Action::AddRemove => {
            if let Some(journal) = journal {
                let origins = crate::engine::execution::delete::missing_origins(
                    journal.journal,
                    journal.identity,
                    &selected,
                    &engine.control(1800),
                )?;
                let gone = origins.values().any(|o| matches!(o, Origin::Missing(_)));
                let removal = removal::build(
                    &saved.catalog,
                    &saved.index,
                    &saved.identity,
                    Some(journal),
                    removal::Request::Missing {
                        sources: gone.then_some(&sources),
                        origins: &origins,
                    },
                )?;
                blockers.extend(removal.blockers.iter().map(|b| format!("removal: {b}")));
                for track in &removal.tracks {
                    match track.decision {
                        removal::Decision::ProposedRemoval => {
                            removed.insert(
                                u64::from_str_radix(&track.track_id, 16)?,
                                ("source_gone", track.missing_source_path.clone(), None),
                            );
                        }
                        removal::Decision::Blocked => blockers.push(format!(
                            "removal: {} is blocked ({})",
                            track.track_id,
                            track.reasons.join(", ")
                        )),
                        removal::Decision::Preserved => {}
                    }
                }
                source_path_unknown = origins
                    .iter()
                    .filter_map(|(track, origin)| match origin {
                        Origin::Unknown(reason) => {
                            Some(json!({"track_id":format!("{track:016x}"),"reason":reason}))
                        }
                        _ => None,
                    })
                    .collect();
            }
        }
        Action::ReplaceChanged => {
            for (track, source) in replace::replacements(&sync)? {
                let cause = match row(&source).map(|r| &r.decision) {
                    Some(Decision::Skip {
                        code: "metadata_update_needs_replacement",
                        ..
                    }) => "field_cleared",
                    _ => "audio_changed",
                };
                ensure!(
                    removed
                        .insert(
                            u64::from_str_radix(&track, 16)?,
                            (cause, None, Some(source))
                        )
                        .is_none(),
                    "track {track} is replaced by two sources"
                );
            }
        }
        Action::ReplaceLibrary => {
            for (path, reason) in additive::uninsertable_sources(&sources) {
                blockers.push(format!(
                    "source {path} cannot be added, so replacing the library would leave it off the phone; exclude it: {reason}"
                ));
                refused.insert(path);
            }
            for track in saved.catalog.tracks() {
                removed.insert(track.sync_id, ("whole_library", None, None));
            }
            for source in &sync.sources {
                if let [only] = &source.matches[..]
                    && let Some(entry) = removed.get_mut(&u64::from_str_radix(&only.track_id, 16)?)
                    && entry.2.is_none()
                {
                    entry.2 = Some(source.relative_path.clone());
                }
            }
        }
    }
    // Each removed track checked against the removal model on this snapshot,
    // as `delete --dry-run` does; each batch checks it again on fresh state.
    let ids: Vec<u64> = removed.keys().copied().collect();
    let mut track_blockers: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    if !ids.is_empty() {
        let scratch = std::env::temp_dir().join(format!(
            "rustunes-change-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_nanos()
        ));
        crate::engine::state::layout::private_directory(&scratch)?;
        let qualified =
            removal_batch::materialize(saved, &scratch.join("sql"), &engine.control(1800))
                .and_then(|sql| removal_batch::qualify_selection(saved, &sql, &ids, DEFAULT_BATCH));
        std::fs::remove_dir_all(&scratch)?;
        let (steps, batches) = qualified?;
        for (track, step) in ids.iter().zip(steps) {
            let reasons: Vec<String> = step["blockers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b.as_str().map(str::to_owned))
                .collect();
            for reason in &reasons {
                blockers.push(format!("removal: {track:016x}: {reason}"));
            }
            track_blockers.insert(*track, reasons);
        }
        for batch in batches {
            blockers.push(format!(
                "removal batch {}: {}",
                batch["batch"],
                batch["blocker"].as_str().unwrap_or_default()
            ));
        }
    }
    let removals: Vec<Removal> = removed
        .iter()
        .map(|(id, (cause, missing, replaced_by))| {
            let track = saved
                .catalog
                .tracks()
                .iter()
                .find(|t| t.sync_id == *id)
                .with_context(|| format!("track {id:016x} is not in the catalogue"))?;
            Ok(Removal {
                track_id: format!("{id:016x}"),
                title: track.metadata.title.clone(),
                artist: track.metadata.artist.clone(),
                album: track.metadata.album.clone(),
                plays: track.playback.play_count_user,
                skips: track.playback.skip_count_user,
                file_bytes: saved
                    .index
                    .tracks()
                    .get(id)
                    .map(|a| a.fingerprint.file_bytes)
                    .or_else(|| saved.index.other_audio().get(id).map(|a| a.file_bytes)),
                cause,
                missing_source_path: missing.clone(),
                replaced_by: replaced_by.clone(),
                blockers: track_blockers.remove(id).unwrap_or_default(),
            })
        })
        .collect::<Result<_>>()?;
    let replaces: BTreeMap<&str, String> = removals
        .iter()
        .filter_map(|r| r.replaced_by.as_deref().map(|s| (s, r.track_id.clone())))
        .collect();
    // An MP3 row carries its tags, not projected fields; an empty value is
    // none, as its projection takes it.
    let names = |row: &additive::SourceDecision| match (&row.projected, &row.mp3) {
        (Some(p), _) => [
            Some(p.title.clone()),
            Some(p.artist.clone()),
            Some(p.album.clone()),
        ],
        (None, Some(s)) => {
            [&s.metadata.title, &s.metadata.artist, &s.metadata.album].map(|values| {
                music_protocol::media::metadata::tag_values(values)
                    .next()
                    .map(str::to_owned)
            })
        }
        (None, None) => [None, None, None],
    };
    let addition = |row: &additive::SourceDecision| {
        let [title, artist, album] = names(row);
        Addition {
            relative_path: row.relative_path.clone(),
            file_bytes: row.source_file_bytes,
            source_file_sha256: row.source_file_sha256.clone(),
            codec: match &row.mp3 {
                Some(_) if row.flac_to_alac => "flac → alac",
                Some(s) => s.codec,
                None => "alac",
            },
            title,
            artist,
            album,
            has_cover: match &row.artwork {
                ArtworkDecision::PreservedExistingTrack {
                    source_cover_sha256,
                    ..
                } => source_cover_sha256.is_some(),
                ArtworkDecision::NoSourceCoverPreserveExisting => false,
                // A changed MP3's cover is decided once its old track is gone.
                ArtworkDecision::UnavailableUntilSourceValidated => {
                    row.mp3.as_ref().is_some_and(|s| {
                        music_protocol::media::mpeg::cover_index(&s.artwork).is_some()
                    })
                }
                _ => true,
            },
            cover_left_out: row
                .validation_issues
                .iter()
                .find(|issue| issue.code == "cover_left_out")
                .map(|issue| issue.message.clone()),
            replaces: replaces.get(row.relative_path.as_str()).cloned(),
        }
    };
    let mut additions = Vec::new();
    let mut updates = Vec::new();
    let mut pending_covers = Vec::new();
    match action {
        Action::RemoveMissing | Action::AddRemove => {
            for row in &sync.sources {
                match &row.decision {
                    Decision::Add { .. } => additions.push(addition(row)),
                    Decision::Update { .. } => updates.push(Update {
                        relative_path: row.relative_path.clone(),
                        decision: serde_json::to_value(&row.decision)?,
                    }),
                    Decision::Conflict { code, message } => blockers.push(format!(
                        "conflict: {}: {code}: {message}",
                        row.relative_path
                    )),
                    Decision::Keep | Decision::Skip { .. } => {}
                }
                if matches!(row.artwork, ArtworkDecision::PendingOwnedCover { .. }) {
                    pending_covers.push(row.relative_path.clone());
                }
            }
        }
        Action::ReplaceChanged => {
            for removal in &removals {
                let source = removal
                    .replaced_by
                    .as_deref()
                    .context("replacement source")?;
                additions.push(addition(row(source).context("replacement source row")?));
            }
            additions.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        }
        // Once every track is gone, each source the replacement can add is
        // added, whatever the phone held.
        Action::ReplaceLibrary => {
            additions.extend(
                sync.sources
                    .iter()
                    .filter(|row| !refused.contains(&row.relative_path))
                    .map(addition),
            );
        }
    }
    // A source the replacement adds again is no skip of it.
    let skipped = sync
        .sources
        .iter()
        .filter(|row| {
            !replaces.contains_key(row.relative_path.as_str())
                && (action != Action::ReplaceLibrary || refused.contains(&row.relative_path))
        })
        .filter_map(|row| match &row.decision {
            Decision::Skip {
                code,
                reason,
                message,
            } => Some(Skipped {
                relative_path: row.relative_path.clone(),
                code,
                reason: reason.clone(),
                detail: message.clone(),
            }),
            _ => None,
        })
        .collect();
    let selection = Scope {
        recursive: options.recursive,
        include: options.include.clone(),
        exclude: options.exclude.clone(),
        selected_files: selected.files().len(),
    };
    let costs = Costs {
        removal_sessions: removals.len().div_ceil(DEFAULT_BATCH),
        removal_batch_size: DEFAULT_BATCH,
        upload_bytes: additions.iter().filter_map(|a| a.file_bytes).sum(),
        plays_lost: removals.iter().map(|r| u64::from(r.plays)).sum(),
        skips_lost: removals.iter().map(|r| u64::from(r.skips)).sum(),
    };
    Ok(ChangePlan {
        schema: SCHEMA,
        action: action.as_str(),
        flac_to_alac,
        source_root: root.to_string_lossy().into_owned(),
        selection,
        device_sha256: hex(&saved.identity.device_identifier_sha256),
        library_sha256: hex(&saved.identity.library_identity_sha256),
        database_sha256: hex(&saved.catalog.source_hashes().database),
        media_anchor: saved.catalog.persisted_media_anchor(),
        device_track_count: saved.catalog.tracks().len(),
        untouched_tracks: saved
            .catalog
            .tracks()
            .len()
            .saturating_sub(removals.len() + updates.len()),
        removals,
        source_path_unknown,
        additions,
        updates,
        skipped,
        pending_covers,
        costs,
        order: ["remove", "sync"],
        dependencies: DEPENDENCIES,
        losses: LOSSES,
        blockers,
        fresh_state_required_before_execution: true,
        execution_authorized_by_plan: false,
    })
}

#[cfg(test)]
mod tests;
