//! Saved-only planning. The returned serialized bytes are exactly the bytes
//! persisted for later reconstruction; a plan never grants write authority.
use super::{Engine, Event, SelectionOptions};
use crate::{engine::state::journal, engine::state::snapshot};
use anyhow::{Result, ensure};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;
use std::{fs::File, path::Path, time::Instant};

pub struct PlanRequest<'a> {
    pub source: &'a Path,
    pub snapshot: &'a Path,
    pub journal: Option<&'a Path>,
    pub output: Option<&'a Path>,
    pub selection: &'a SelectionOptions,
    pub add_only: bool,
    /// `--flac-to-alac`: plan FLAC additions as ALACs converted from them.
    pub flac_to_alac: bool,
}
pub struct PlanResult {
    pub add_only: bool,
    pub plan: additive::Plan,
    pub serialized: Vec<u8>,
    pub additions: usize,
    pub matched: usize,
    pub updates: usize,
    pub conflicts: usize,
    /// Sources the plan leaves out (`additive::skip`); the rest goes on
    /// without them.
    pub skipped: usize,
    pub source_exclusions: usize,
}
impl PlanResult {
    pub fn ready(&self) -> bool {
        !self.plan.has_conflicts()
    }
}

pub(super) fn run(request: PlanRequest<'_>, engine: &Engine) -> Result<PlanResult> {
    engine.check()?;
    if let Some(output) = request.output {
        ensure!(
            !output.try_exists()?,
            "plan output already exists; choose a new file"
        );
    }
    engine.emit(Event::ReadingSavedSnapshot)?;
    let snapshot = snapshot::open(request.snapshot)?;
    let library = identity::parse(&snapshot.raw_preferences)?;
    let identity =
        journal::Identity::for_saved_device(snapshot.identity.device_identifier_sha256, &library)?;
    let journal = request
        .journal
        .map(|p| journal::Journal::open_read_only(p, &identity))
        .transpose()?;
    let selection = source_collection::select(request.source, request.selection)?;
    let mut last = Instant::now();
    let mut sources = source_collection::inspect_with_progress(&selection, |done, total| {
        engine.check()?;
        if done == 0 || done == total || last.elapsed().as_secs() >= 5 {
            engine.emit(Event::InspectingSources { done, total })?;
            last = Instant::now();
        }
        Ok(())
    })?;
    sources.flac_to_alac = request.flac_to_alac;
    let candidates = candidates::generate(&sources, &snapshot.catalog, journal.as_ref())?;
    engine.emit(Event::ComparingSources)?;
    let plan = additive::build_from_snapshot(
        &sources,
        &snapshot,
        journal.as_ref().map(|journal| additive::JournalEvidence {
            journal,
            identity: &identity,
        }),
        &candidates,
        request.add_only,
    )?;
    let counts = plan.decision_counts();
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
    Ok(PlanResult {
        add_only: request.add_only,
        plan,
        serialized,
        additions: counts.additions,
        matched: counts.matched,
        updates: counts.updates,
        conflicts: counts.conflicts,
        skipped: counts.skipped,
        source_exclusions: sources.exclusions.len(),
    })
}

pub(crate) mod additive;
pub(crate) mod candidates;
pub(crate) mod collections;
pub(crate) mod metadata_edit;
pub(crate) mod removal;
pub(crate) mod saved_plan;
pub(crate) mod source_collection;
pub(crate) mod source_reads;
pub(crate) mod source_validation;
