//! Exact additive dry-run decisions from complete, identity-bound readback.
//! Planning reserves nothing and authorizes no phone mutation or deletion.
//! A proposed add still needs a durable journal preparation, fresh catalog/Ready,
//! and fresh AFC namespace checks. Rebind each sequential step without silently
//! replacing its proposed identities, fields, or permanent destination.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    engine::planning::collections::{CandidateIds, PlannedTrack, Planner},
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::planning::source_collection::{SourceCollection, SourceTrack},
    engine::planning::source_validation,
    engine::state::audio_index::{DeviceAudioIndex, IndexIdentity},
    engine::state::journal::{AttemptState, Binding, Identity, Journal},
};
use music_protocol::catalogue::sqlite::{Catalog, Track};

pub struct Candidate {
    pub ids: CandidateIds,
    pub path: String,
}

#[derive(Clone, Copy)]
pub struct JournalEvidence<'a> {
    pub journal: &'a Journal,
    pub identity: &'a Identity,
}

#[derive(Serialize)]
pub struct Plan {
    pub schema: &'static str,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "is_false")]
    pub preserve_existing_tracks: bool,
    /// `--flac-to-alac`: a FLAC addition goes as an ALAC converted from it.
    #[serde(skip_serializing_if = "is_false")]
    pub flac_to_alac: bool,
    pub source_root: String,
    pub selection: SelectionScope,
    pub exclusions: Vec<ExcludedSource>,
    pub metadata_comparison_scope: MetadataComparisonScope,
    pub device_sha256: String,
    pub library_sha256: String,
    pub database_sha256: String,
    pub wal_sha256: Option<String>,
    pub media_anchor: u32,
    pub device_track_count: usize,
    pub journal_blockers: Vec<String>,
    /// Potential receiver-side import effect; never an instruction to edit
    /// counters. Omitted for plans that propose no artwork write or no reset.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artwork_import_recent_play_resets: Vec<Value>,
    /// When present, both recent counters follow the complete paired reset rule.
    /// Omitted when no proposed artwork write resets a skip counter.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artwork_import_recent_skip_resets: Vec<Value>,
    pub sources: Vec<SourceDecision>,
    /// Includes unmatched other-codec rows. No omission is a removal request.
    pub preserved_unmatched_device_tracks: Vec<String>,
    /// Always 0: an additive plan removes nothing.
    pub phone_deletions: u32,
    /// Always false: a plan alone authorizes no write.
    pub execution_authorized_by_plan: bool,
}

#[derive(Serialize)]
pub struct SelectionScope {
    pub base_directory: String,
    pub recursive: bool,
    pub include: BTreeSet<String>,
    pub exclude: BTreeSet<String>,
    pub selected_files: usize,
    pub selected_bytes: u64,
}

#[derive(Serialize)]
pub struct ExcludedSource {
    pub relative_path: String,
    pub reason: &'static str,
    pub descendants_unscanned: bool,
}

#[derive(Serialize)]
pub struct MetadataComparisonScope {
    pub compared_fields: Vec<&'static str>,
    /// Raw rating/compilation values may be compared while their receiver
    /// interpretation and source-driven update behavior remain unqualified.
    pub unverified_fields: Vec<&'static str>,
    pub ascii_case_equivalent_group_fields: Vec<&'static str>,
    pub container_byte_drift_reported_separately: bool,
}

impl Plan {
    /// Existing execution-blocker predicate, including blocked descriptive
    /// Updates. Actual Conflict and Update counts are reported independently.
    /// Every addition still needs per-step fresh state, journal and guards.
    pub fn has_conflicts(&self) -> bool {
        !self.journal_blockers.is_empty()
            || self
                .sources
                .iter()
                .any(|s| matches!(s.decision, Decision::Conflict { .. }))
    }
}

impl SourceDecision {
    /// The cover bytes a sync sends again for this kept or updated ALAC it
    /// manages when no binding proves the phone's cover (each in a phone
    /// session of its own): what sync's cover phase selects. Add-only sends
    /// none but pending ones, which a plan names apart.
    pub fn cover_resent(&self, add_only: bool) -> Option<usize> {
        let managed = matches!(&self.matches[..], [m] if m.ownership == "inserted_by_us");
        if add_only
            || self.mp3.is_some()
            || !managed
            || !matches!(self.decision, Decision::Keep | Decision::Update { .. })
        {
            return None;
        }
        match self.artwork {
            ArtworkDecision::ExistingNeedsVerification { bytes, .. }
            | ArtworkDecision::SeparateUpdateNeeded { bytes, .. } => Some(bytes),
            ArtworkDecision::PreservedExistingTrack {
                source_cover_bytes, ..
            } => source_cover_bytes,
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct DecisionCounts {
    pub additions: usize,
    pub matched: usize,
    pub updates: usize,
    pub conflicts: usize,
    pub skipped: usize,
}
impl Plan {
    pub fn decision_counts(&self) -> DecisionCounts {
        let mut result = DecisionCounts::default();
        for row in &self.sources {
            match row.decision {
                Decision::Add { .. } => result.additions += 1,
                Decision::Keep => result.matched += 1,
                Decision::Update { .. } => result.updates += 1,
                Decision::Conflict { .. } => result.conflicts += 1,
                Decision::Skip { .. } => result.skipped += 1,
            }
        }
        result
    }
}

mod managed_source;
pub(crate) mod skip;
pub(crate) mod source_metadata;
pub(crate) mod update;

pub(crate) fn metadata_catalogue_fields(catalog: &Catalog, track: &Track) -> serde_json::Value {
    source_metadata::current_fields(catalog, track)
}

mod managed_audio;
pub use managed_audio::ManagedAudioReplacement;
pub(crate) use managed_audio::source_paths;

#[derive(Serialize)]
pub struct SourceDecision {
    pub relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_audio_replacement: Option<ManagedAudioReplacement>,
    pub source_file_bytes: Option<u64>,
    pub source_file_sha256: Option<String>,
    pub encoded_audio_sha256: Option<String>,
    pub projected: Option<ProjectedFields>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp3: Option<music_protocol::media::mpeg::Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp3_insertion:
        Option<crate::engine::planning::source_collection::mp3::preparation::Assessment>,
    /// This FLAC's addition sends an ALAC converted from it.
    #[serde(skip_serializing_if = "is_false")]
    pub flac_to_alac: bool,
    pub validation_issues: Vec<source_validation::Issue>,
    pub matches: Vec<ExistingMatch>,
    pub artwork: ArtworkDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_composer: Option<NativeComposer>,
    #[serde(flatten)]
    pub decision: Decision,
}

#[derive(Serialize)]
pub struct NativeComposer {
    pub requested_label: String,
    pub wire_label: String,
    pub wire_sort: String,
    pub existing_lookup_id: i64,
}

pub use music_protocol::media::metadata::ProjectedFields;

#[derive(Serialize)]
pub struct ExistingMatch {
    pub track_id: String,
    pub wire_id: String,
    pub path: String,
    pub codec: &'static str,
    pub encoded_audio_equal: bool,
    pub full_container_equal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_basis: Option<&'static str>,
    pub ownership: &'static str,
    pub catalog_differences: Vec<FieldDifference>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub canonicalized_fields: Vec<FieldDifference>,
    /// The phone's file of an owned MP3 matched by its frames: the bytes this
    /// tool sent, which an Update leaves as they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_file_sha256: Option<String>,
}

#[derive(Serialize)]
pub struct FieldDifference {
    pub field: &'static str,
    pub current: Value,
    pub desired: Value,
}

#[derive(Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Add {
        track_id: String,
        wire_id: String,
        album: Group,
        artist: Group,
        path: String,
        pending_collection_ids: Vec<String>,
        fresh_state_required_before_execution: bool,
    },
    /// A track this tool inserted whose catalogue fields differ from its
    /// source's tags; `sync` sends the edit in verified metadata batches. The
    /// audio is the same, so the file on the phone is kept.
    Update {
        track_id: String,
        changes: Vec<FieldDifference>,
    },
    /// Audio/catalog fields agree. Artwork and container drift remain explicit.
    Keep,
    Conflict {
        code: &'static str,
        message: String,
    },
    /// Left off the phone without holding the rest: a refusal about this
    /// source alone (`skip`), in plain words (`reason`) and in full
    /// (`message`). Its phone track, if it has one, stays as it is.
    Skip {
        code: &'static str,
        reason: String,
        message: String,
    },
}

#[derive(Serialize)]
pub struct Group {
    pub id: String,
    pub wire_id: String,
    pub status: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ArtworkDecision {
    /// Finish this tool's interrupted insertion without replacing any cover.
    PendingOwnedCover {
        sha256: String,
        bytes: usize,
    },
    PreservedExistingTrack {
        source_cover_sha256: Option<String>,
        source_cover_bytes: Option<usize>,
        observed_cache_ids: Vec<u32>,
    },
    NoSourceCoverPreserveExisting,
    SeparateUpdateNeeded {
        sha256: String,
        bytes: usize,
    },
    VerifiedJournalMatch {
        sha256: String,
        bytes: usize,
        cache_id: u32,
    },
    ExistingNeedsVerification {
        sha256: String,
        bytes: usize,
        observed_cache_ids: Vec<u32>,
    },
    UnavailableUntilSourceValidated,
}

/// Candidates are caller-generated random identities and exact permanent paths,
/// keyed by source relative path. Candidates for Keep/Conflict rows are unused.
#[cfg(test)]
pub fn build(
    sources: &SourceCollection,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    identity: &IndexIdentity,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
) -> Result<Plan> {
    build_with_policy(
        sources, catalog, index, identity, journal, candidates, false,
    )
}

/// Add-only preserves matches, except an explicitly planned missing cover for
/// this tool's own insertion with the exact original source and no prior cover.
#[cfg(test)]
pub fn build_with_policy(
    sources: &SourceCollection,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    identity: &IndexIdentity,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
    preserve_existing_tracks: bool,
) -> Result<Plan> {
    build_inner(
        sources,
        catalog,
        index,
        identity,
        journal,
        candidates,
        preserve_existing_tracks,
    )
}

/// Plans from a saved snapshot's catalogue and audio index.
pub fn build_from_snapshot(
    sources: &SourceCollection,
    snapshot: &crate::engine::state::snapshot::Snapshot,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
    preserve_existing_tracks: bool,
) -> Result<Plan> {
    build_inner(
        sources,
        &snapshot.catalog,
        &snapshot.index,
        &snapshot.identity,
        journal,
        candidates,
        preserve_existing_tracks,
    )
}

/// Mixed execution replans retain the caller's absolute deadline and Arc.
pub(crate) fn build_from_snapshot_with_control(
    sources: &SourceCollection,
    snapshot: &crate::engine::state::snapshot::Snapshot,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
    preserve_existing_tracks: bool,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<Plan> {
    control.check()?;
    build_inner_with_control(
        sources,
        &snapshot.catalog,
        &snapshot.index,
        &snapshot.identity,
        journal,
        candidates,
        preserve_existing_tracks,
        Some(control),
    )
}

fn build_inner(
    sources: &SourceCollection,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    identity: &IndexIdentity,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
    preserve_existing_tracks: bool,
) -> Result<Plan> {
    build_inner_with_control(
        sources,
        catalog,
        index,
        identity,
        journal,
        candidates,
        preserve_existing_tracks,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_inner_with_control(
    sources: &SourceCollection,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    identity: &IndexIdentity,
    journal: Option<JournalEvidence<'_>>,
    candidates: &BTreeMap<String, Candidate>,
    preserve_existing_tracks: bool,
    control: Option<&crate::engine::execution::native_insert::Control>,
) -> Result<Plan> {
    if let Some(control) = control {
        control.check()?;
    }
    index.ensure_matches(catalog, identity)?;
    ensure!(
        sources.inspected_len() != 0 || !sources.failures.is_empty(),
        "empty source selection cannot produce a sync plan"
    );
    ensure!(
        sources.inspected_len() + sources.failures.len() <= 4096,
        "source plan exceeds bound"
    );
    ensure!(
        sources.selection_stats.selected_files == sources.inspected_len() + sources.failures.len(),
        "source inspection does not cover its complete selected scope"
    );
    let paths = catalog.music_paths()?;
    // Folders tracks use, and the phone's own registry: an emptied library
    // keeps its folders but no track names them.
    let folders: BTreeSet<_> = paths
        .iter()
        .filter_map(|p| p.rsplit_once('/').map(|(parent, _)| parent.to_owned()))
        .chain(catalog.music_folders().iter().cloned())
        .collect();
    let journal_evidence = journal.as_ref().map(|e| (e.journal, e.identity));
    let mut managed_context = None;
    let mut journal = JournalView::read(journal, identity, index)?;
    let mut planner = Planner::new(catalog, &journal.reserved_ids);
    let mut reserved_paths: BTreeSet<_> = paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    reserved_paths.append(&mut journal.reserved_paths);
    let device: BTreeMap<_, _> = catalog.tracks().iter().map(|t| (t.sync_id, t)).collect();
    let mut audio: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
    for track in index.tracks().values() {
        audio
            .entry(track.fingerprint.audio.sha256)
            .or_default()
            .push(track.sync_id);
    }
    let mut source_audio: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    let mut names = BTreeSet::new();
    for source in &sources.tracks {
        if let Some(control) = control {
            control.check()?;
        }
        validate_relative(&source.relative_path)?;
        ensure!(
            names.insert(&source.relative_path),
            "duplicate source path in plan"
        );
        *source_audio
            .entry(source.fingerprint.audio.sha256)
            .or_default() += 1;
    }
    for source in &sources.mp3_tracks {
        if let Some(control) = control {
            control.check()?;
        }
        validate_relative(&source.relative_path)?;
        ensure!(
            names.insert(&source.relative_path),
            "duplicate source path in plan"
        );
    }
    for failed in &sources.failures {
        if let Some(control) = control {
            control.check()?;
        }
        validate_relative(&failed.relative_path)?;
        ensure!(
            names.insert(&failed.relative_path),
            "duplicate source path in plan"
        );
    }
    let mut matched = BTreeSet::new();
    let mut rows = Vec::new();
    let mut sorted: Vec<_> = sources.tracks.iter().collect();
    sorted.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    for source in sorted {
        let exact = audio
            .get(&source.fingerprint.audio.sha256)
            .cloned()
            .unwrap_or_default();
        matched.extend(exact.iter().copied());
        let mut row = source_row(source);
        let projected = ProjectedFields::from_source(&source.metadata);
        let report =
            source_validation::inspect(&sources.base_directory.join(&source.relative_path), false);
        let revalidation = check_revalidation(source, &report);
        let source_supported = report.supported_by_current_profile;
        row.validation_issues = report.issues;
        if let Ok(fields) = &projected {
            row.projected = Some(fields.clone());
        }
        for id in &exact {
            row.matches.push(existing_match(
                source,
                device[id],
                index,
                &journal,
                true,
                projected.as_ref().ok(),
                catalog,
            ));
        }
        // Only a source this run could add again is a replacement.
        if !preserve_existing_tracks
            && source_supported
            && revalidation.is_ok()
            && source_audio[&source.fingerprint.audio.sha256] == 1
            && let Some(fields) = row.projected.as_ref()
            && let Some((j, identity)) = journal_evidence
        {
            row.managed_audio_replacement = managed_audio::describe(
                &mut managed_context,
                JournalEvidence {
                    journal: j,
                    identity,
                },
                &journal.active_bindings,
                sources,
                source,
                fields,
                catalog,
                index,
                control,
            );
        }
        if let Err(error) = revalidation {
            row.decision = conflict("source_changed_or_invalid", error.to_string());
        } else if let Some(description) = row
            .managed_audio_replacement
            .as_ref()
            .filter(|description| description.blocks())
        {
            let cover = report
                .artwork
                .as_ref()
                .map(|art| (art.sha256.as_str(), art.bytes));
            row.artwork = artwork_decision(cover, &exact, catalog, index, &journal);
            // A blocked replacement never enters the additive planner: later
            // rows must not reuse groups allocated for a nonexecuting proposal.
            row.decision = conflict(
                if description.replacement_identity_established {
                    "managed_audio_replacement_proposed"
                } else {
                    "managed_audio_replacement_evidence_refused"
                },
                description.refusal.clone().unwrap_or_else(|| "Changed audio at the source path of a track this tool inserted; sync with --replace-changed replaces the track (removes it, then adds the new audio)".into()),
            );
        } else if preserve_existing_tracks
            && exact.len() == 1
            && source_audio[&source.fingerprint.audio.sha256] == 1
            && projected.is_ok()
            && row.validation_issues.iter().all(|issue| {
                issue.severity != source_validation::Severity::Error
                    || issue.code == "compilation_grouping_unverified"
            })
        {
            // Compilation grouping limits insertion; they cannot require a
            // rewrite of an independently matched existing audio file.
            row.decision = Decision::Keep;
            row.artwork = ArtworkDecision::PreservedExistingTrack {
                source_cover_sha256: report.artwork.as_ref().map(|a| a.sha256.clone()),
                source_cover_bytes: report.artwork.as_ref().map(|a| a.bytes),
                observed_cache_ids: exact
                    .iter()
                    .filter_map(|id| catalog.artwork().track_cache_ids().get(id).copied())
                    .collect(),
            };
            if source_supported
                && let Some(cover) = &report.artwork
                && let Some(binding) = journal.binding(exact[0], index)
                && owes_cover(
                    catalog,
                    exact[0],
                    &source.fingerprint,
                    binding,
                    !journal.blockers.is_empty(),
                )
            {
                row.artwork = ArtworkDecision::PendingOwnedCover {
                    sha256: cover.sha256.clone(),
                    bytes: cover.bytes,
                };
            }
        } else if projected.is_err()
            || (!source_supported
                && !(exact.len() == 1
                    && row.validation_issues.iter().all(|issue| {
                        issue.severity != source_validation::Severity::Error
                            || issue.code == "compilation_grouping_unverified"
                    })))
        {
            // Name what failed, as validate does.
            let mut reasons: Vec<String> = row
                .validation_issues
                .iter()
                .filter(|issue| issue.severity == source_validation::Severity::Error)
                .map(|issue| issue.message.clone())
                .collect();
            if reasons.is_empty()
                && let Err(error) = &projected
            {
                reasons.push(format!("{error:#}"));
            }
            row.decision = conflict(
                "unsupported_source",
                format!(
                    "Source validation/projection must pass before planning: {}",
                    reasons.join("; ")
                ),
            );
        } else {
            let projected = projected?;
            let cover = report
                .artwork
                .as_ref()
                .map(|art| (art.sha256.as_str(), art.bytes));
            row.artwork = artwork_decision(cover, &exact, catalog, index, &journal);
            if source_audio[&source.fingerprint.audio.sha256] > 1 {
                row.decision = conflict(
                    "duplicate_source_audio",
                    "Select one source variant explicitly; no identity allocated".into(),
                );
            } else if exact.len() > 1 {
                row.decision = conflict(
                    "multiple_existing_audio_matches",
                    "Every existing audio match is preserved; no automatic identity selection"
                        .into(),
                );
            } else if exact.len() == 1 {
                let owned = journal.binding(exact[0], index).is_some();
                let differs = row
                    .matches
                    .iter()
                    .any(|m| !m.catalog_differences.is_empty());
                row.decision = if preserve_existing_tracks || !differs {
                    Decision::Keep
                } else if !owned {
                    conflict(
                        "existing_metadata_update_unqualified",
                        "Encoded audio is present, but source metadata Update requires admitted inserted ownership".into(),
                    )
                } else {
                    let changes = &row.matches[0].catalog_differences;
                    match update::from_differences(&projected, device[&exact[0]], changes) {
                        Ok(Some(_)) => Decision::Update {
                            track_id: pid(exact[0]),
                            changes: changes
                                .iter()
                                .map(|d| FieldDifference {
                                    field: d.field,
                                    current: d.current.clone(),
                                    desired: d.desired.clone(),
                                })
                                .collect(),
                        },
                        Ok(None) => Decision::Keep,
                        Err(error) => update_refused(&error),
                    }
                };
            } else {
                for track in catalog
                    .tracks()
                    .iter()
                    .filter(|track| plausible(&projected, track))
                {
                    row.matches.push(existing_match(
                        source,
                        track,
                        index,
                        &journal,
                        false,
                        Some(&projected),
                        catalog,
                    ));
                }
                let composer = native_composer(catalog, projected.composer.as_deref());
                if let Err(error) = &composer {
                    row.decision = conflict("composer_lookup_unqualified", error.to_string());
                } else if !row.matches.is_empty() {
                    row.decision = conflict("plausible_existing_audio_requires_comparison", "Encoded mismatch is not absence: plausible metadata/duration requires optional PCM comparison or explicit review; titles establish no identity".into());
                } else if !journal.blockers.is_empty() {
                    row.decision = conflict("journal_reconciliation_required", "Outstanding intent or drifted verified binding requires reconciliation before any new identity proposal".into());
                } else if let Some(candidate) = candidates.get(&source.relative_path) {
                    row.native_composer = composer?;
                    row.decision = match plan_add(
                        &mut planner,
                        source,
                        candidate,
                        &folders,
                        &mut reserved_paths,
                    ) {
                        Ok(decision) => decision,
                        Err(error) => conflict("candidate_or_grouping_conflict", error.to_string()),
                    };
                } else {
                    row.decision = conflict("candidate_required", "Missing encoded audio still needs caller-selected random identities and an exact new destination".into());
                }
            }
        }
        rows.push(row);
    }
    for source in &sources.mp3_tracks {
        if let Some(control) = control {
            control.check()?;
        }
        let mut row = mp3_row(
            source,
            sources,
            catalog,
            index,
            &journal,
            preserve_existing_tracks,
        )?;
        let unmatched = matches!(
            row.decision,
            Decision::Conflict {
                code: "mp3_insertion_unqualified",
                ..
            }
        );
        // A source this run cannot send is left out whole, so it is never a
        // replacement either: an earlier version's track stays.
        let prepared = unmatched.then(|| {
            if let Some(refusal) = unconvertible(sources, &source.source) {
                return Err(conflict("flac_conversion_refused", format!("{refusal:#}")));
            }
            let prepared = PreparedSource::read_expected(
                &sources.base_directory.join(&source.relative_path),
                &source.source,
            )
            .map_err(|e| conflict("source_changed_or_invalid", format!("{e:#}")))?;
            prepared
                .ensure_batch_insertion_allowed()
                .map_err(|e| conflict("mp3_source_profile_unsupported", format!("{e:#}")))?;
            Ok(prepared)
        });
        if matches!(prepared, Some(Ok(_)))
            && !preserve_existing_tracks
            && let Some((j, identity)) = journal_evidence
        {
            row.managed_audio_replacement = managed_audio::describe_mpeg(
                &mut managed_context,
                JournalEvidence {
                    journal: j,
                    identity,
                },
                &journal.active_bindings,
                &journal.conversions,
                sources,
                source,
                catalog,
                control,
            );
        }
        if let Some(description) = row
            .managed_audio_replacement
            .as_ref()
            .filter(|description| description.blocks())
        {
            row.decision = conflict(
                if description.replacement_identity_established {
                    "managed_audio_replacement_proposed"
                } else {
                    "managed_audio_replacement_evidence_refused"
                },
                description.refusal.clone().unwrap_or_else(|| format!("Changed audio at the source path of a track this tool inserted from a {} file; sync with --replace-changed replaces the track (removes it, then adds the new audio)", source.source.format().label())),
            );
        } else if let Some(prepared) = prepared {
            let converts = sources.flac_to_alac
                && source.source.format() == music_protocol::media::format::Format::Flac;
            row.decision = match prepared {
                Err(refusal) => refusal,
                Ok(prepared) => {
                    if !journal.blockers.is_empty() {
                        conflict(
                            "journal_reconciliation_required",
                            "Reconcile pending or drifted bindings before MPEG allocation".into(),
                        )
                    } else if journal_evidence.is_none() {
                        conflict(
                            "mp3_journal_required",
                            format!(
                                "{} additions go in an insert batch, which needs the existing idle journal (plan with --journal).",
                                prepared.source().format().label()
                            ),
                        )
                    } else if let Some(plausible) = plausible_mpeg(&prepared, catalog) {
                        conflict(
                            "plausible_existing_audio_requires_comparison",
                            format!(
                                "Encoded mismatch is not absence: track {plausible} has this source's title, its artist or album and its duration; it may be an earlier version of it. Titles establish no identity, so nothing is added"
                            ),
                        )
                    } else if let Some(candidate) = candidates.get(&source.relative_path) {
                        match crate::engine::state::journal::mpeg_insert::ensure_new_source(
                            &prepared,
                            index,
                            &journal.active_bindings,
                        )
                        .and_then(|()| {
                            plan_mpeg_add(
                                &mut planner,
                                &prepared,
                                converts,
                                candidate,
                                &folders,
                                &mut reserved_paths,
                            )
                        }) {
                            Ok(decision) => {
                                let cover = music_protocol::media::mpeg::cover_picture(
                                    &source.source.artwork,
                                )
                                .map(|p| (p.sha256.as_str(), p.bytes));
                                row.artwork =
                                    artwork_decision(cover, &[], catalog, index, &journal);
                                row.flac_to_alac = converts;
                                if let Some(report) = row.mp3_insertion.as_mut() {
                                    report.insertion_route = Some("insert_batch");
                                    report.required_qualification = if converts {
                                        vec![
                                            "checked_lossless_flac_to_alac_conversion",
                                            "fresh_complete_state_before_each_batch",
                                            "original_native_transfer_and_full_readback",
                                            "observed_insert_batch_settlement",
                                        ]
                                    } else {
                                        vec![
                                            "fresh_complete_state_before_each_batch",
                                            "original_native_transfer_and_full_readback",
                                            "observed_insert_batch_settlement",
                                        ]
                                    };
                                }
                                decision
                            }
                            Err(e) => {
                                conflict("mp3_candidate_or_grouping_conflict", format!("{e:#}"))
                            }
                        }
                    } else {
                        conflict(
                            "candidate_required",
                            "Missing audio needs an exact new identity and destination".into(),
                        )
                    }
                }
            };
        }
        matched.extend(
            row.matches
                .iter()
                .map(|m| u64::from_str_radix(&m.track_id, 16))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        rows.push(row);
    }
    for failed in &sources.failures {
        if let Some(control) = control {
            control.check()?;
        }
        rows.push(SourceDecision {
            relative_path: failed.relative_path.clone(),
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
            artwork: ArtworkDecision::UnavailableUntilSourceValidated,
            native_composer: None,
            decision: conflict("source_inspection_failed", failed.message.clone()),
        });
    }
    rows.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    skip::leave_out(&mut rows);
    let (recent_play_resets, recent_skip_resets) = artwork_recent_reset_disclosure(&rows, catalog);
    if let (Some(Ok(context)), Some(_)) = (&managed_context, journal_evidence) {
        // The held managed ancestry still needs a final publication recheck.
        context.recheck()?;
    }
    if let Some(control) = control {
        control.check()?;
    }
    Ok(Plan {
        schema: "rustunes.additive-plan",
        mode: "additive_dry_run",
        preserve_existing_tracks,
        flac_to_alac: sources.flac_to_alac,
        source_root: sources
            .root
            .to_str()
            .context("source root must be UTF-8")?
            .to_owned(),
        selection: SelectionScope {
            base_directory: sources
                .base_directory
                .to_str()
                .context("source base directory must be UTF-8")?
                .to_owned(),
            recursive: sources.options.recursive,
            include: sources.options.include.clone(),
            exclude: sources.options.exclude.clone(),
            selected_files: sources.selection_stats.selected_files,
            selected_bytes: sources.selection_stats.selected_bytes,
        },
        exclusions: sources
            .exclusions
            .iter()
            .map(|e| ExcludedSource {
                relative_path: e.relative_path.clone(),
                reason: e.reason.as_str(),
                descendants_unscanned: e.descendants_unscanned,
            })
            .collect(),
        metadata_comparison_scope: MetadataComparisonScope {
            compared_fields: vec![
                "title",
                "sort_name",
                "content_rating",
                "content_rating_level",
                "compilation",
                "artist",
                "album",
                "album_artist",
                "genre",
                "composer",
                "copyright",
                "year",
                "track",
                "track_total",
                "disc",
                "disc_total",
                "total_time_ms",
            ],
            unverified_fields: vec!["content_rating", "compilation"],
            ascii_case_equivalent_group_fields: vec!["artist", "album", "album_artist"],
            container_byte_drift_reported_separately: true,
        },
        device_sha256: hex(&identity.device_identifier_sha256),
        library_sha256: hex(&identity.library_identity_sha256),
        database_sha256: hex(&catalog.source_hashes().database),
        wal_sha256: catalog.source_hashes().wal.as_ref().map(hex),
        media_anchor: catalog.persisted_media_anchor(),
        device_track_count: catalog.tracks().len(),
        journal_blockers: journal.blockers,
        artwork_import_recent_play_resets: recent_play_resets,
        artwork_import_recent_skip_resets: recent_skip_resets,
        sources: rows,
        preserved_unmatched_device_tracks: device
            .keys()
            .filter(|id| !matched.contains(id))
            .map(|id| pid(*id))
            .collect(),
        phone_deletions: 0,
        execution_authorized_by_plan: false,
    })
}

/// Full-file equality proves the same MPEG bytes; the device index records no
/// MPEG frame digest. No tag-only audio equality is inferred here.
fn mp3_row(
    source: &crate::engine::planning::source_collection::Mp3Track,
    sources: &SourceCollection,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    journal: &JournalView,
    preserve_existing_tracks: bool,
) -> Result<SourceDecision> {
    let f = &source.source.fingerprint;
    let exact: Vec<_> = index
        .other_audio()
        .values()
        .filter(|t| t.file_bytes == f.file_bytes && hex(&t.full_file_sha256) == f.full_file_sha256)
        .collect();
    let fresh =
        music_protocol::media::mpeg::read(&sources.base_directory.join(&source.relative_path));
    let revalidated = fresh.as_ref().is_ok_and(|s| s == &source.source);
    let duplicate = sources
        .mp3_tracks
        .iter()
        .filter(|t| t.source.fingerprint.audio == f.audio)
        .count()
        > 1;
    // A track this tool inserted from the same frames, whose source has since
    // changed only outside them (its tags or cover): the phone still holds the
    // file that was sent, and its catalogue fields can follow the new tags.
    let retagged = if exact.is_empty() && revalidated {
        owned_mpeg_audio(&source.source, journal, index)
    } else {
        Vec::new()
    };
    // A FLAC this tool sent as an ALAC converted from it: the journal links
    // the FLAC's audio to that track, whether or not this run converts.
    let converted = if exact.is_empty() && retagged.is_empty() && revalidated {
        converted_flac_audio(&source.source, journal, index)
    } else {
        Vec::new()
    };
    let desired = (!retagged.is_empty() || !converted.is_empty()).then(|| {
        PreparedSource::read_expected(
            &sources.base_directory.join(&source.relative_path),
            &source.source,
        )
        .and_then(|prepared| prepared.projected())
    });
    let mut matches = Vec::new();
    for audio in &exact {
        let track = catalog
            .tracks()
            .iter()
            .find(|t| t.sync_id == audio.sync_id)
            .context("MPEG index target absent")?;
        let mut diffs = Vec::new();
        for (field, current, values) in [
            (
                "title",
                &track.metadata.title,
                &source.source.metadata.title,
            ),
            (
                "artist",
                &track.metadata.artist,
                &source.source.metadata.artist,
            ),
            (
                "album",
                &track.metadata.album,
                &source.source.metadata.album,
            ),
        ] {
            // An empty value is no value (`metadata::tag_value`).
            let values: Vec<_> = music_protocol::media::metadata::tag_values(values).collect();
            if values.len() != 1 || values[0] != current {
                diffs.push(FieldDifference {
                    field,
                    current: json!(current),
                    desired: json!(values),
                });
            }
        }
        matches.push(ExistingMatch {
            track_id: pid(audio.sync_id),
            wire_id: wire(audio.sync_id),
            path: audio.afc_path.clone(),
            codec: audio.codec.format().codec_label(),
            encoded_audio_equal: true,
            full_container_equal: true,
            match_basis: Some("full_file_equality_with_mpeg_layout"),
            ownership: if journal.binding(audio.sync_id, index).is_some() {
                "inserted_by_us"
            } else {
                "unowned"
            },
            catalog_differences: diffs,
            canonicalized_fields: Vec::new(),
            device_file_sha256: None,
        });
    }
    for binding in &retagged {
        let track = catalog
            .tracks()
            .iter()
            .find(|t| t.sync_id == binding.content.track_pid)
            .context("MPEG binding target absent")?;
        let (catalog_differences, canonicalized_fields) = match &desired {
            Some(Ok(fields)) => compared_fields(fields, catalog, track),
            _ => Default::default(),
        };
        matches.push(ExistingMatch {
            track_id: pid(track.sync_id),
            wire_id: wire(track.sync_id),
            path: binding.content.path.clone(),
            codec: binding.content.source.format().codec_label(),
            encoded_audio_equal: true,
            full_container_equal: false,
            match_basis: Some("owned_mpeg_frame_identity"),
            ownership: "inserted_by_us",
            catalog_differences,
            canonicalized_fields,
            device_file_sha256: Some(hex(&binding.content.source.full_file_sha256)),
        });
    }
    for binding in &converted {
        let track = catalog
            .tracks()
            .iter()
            .find(|t| t.sync_id == binding.content.track_pid)
            .context("converted FLAC's binding target absent")?;
        let (catalog_differences, canonicalized_fields) = match &desired {
            Some(Ok(fields)) => compared_fields(fields, catalog, track),
            _ => Default::default(),
        };
        matches.push(ExistingMatch {
            track_id: pid(track.sync_id),
            wire_id: wire(track.sync_id),
            path: binding.content.path.clone(),
            codec: binding.content.source.format().codec_label(),
            encoded_audio_equal: false,
            full_container_equal: false,
            match_basis: Some("flac_conversion"),
            ownership: "inserted_by_us",
            catalog_differences,
            canonicalized_fields,
            device_file_sha256: Some(hex(&binding.content.source.full_file_sha256)),
        });
    }
    let shape_matches = exact.iter().all(|t| {
        let crate::engine::state::audio_index::OtherCodec::Mpeg1Layer3 {
            sample_rate,
            channels,
            frame_count,
            audio_offset,
            audio_bytes,
            trailing_tag_bytes,
        } = t.codec
        else {
            let crate::engine::state::audio_index::OtherCodec::Encoded(encoded) = &t.codec else {
                return false;
            };
            return source
                .source
                .stream
                .as_ref()
                .is_some_and(|s| s.encoded == *encoded);
        };
        sample_rate == f.audio.sample_rate
            && channels == f.audio.channels
            && frame_count == f.audio.frame_count
            && audio_offset == source.source.audio_offset
            && audio_bytes == f.audio.audio_bytes
            && trailing_tag_bytes == source.source.trailing_tag_bytes
    });
    let preparation = if exact.is_empty() && revalidated {
        Some(crate::engine::planning::source_collection::mp3::preparation::assess(&source.source))
    } else {
        None
    };
    let decision = if !shape_matches {
        conflict(
            "device_mpeg_classification_conflict",
            "Full-file identity and saved MPEG frame layout disagree".into(),
        )
    } else if !revalidated {
        conflict(
            "source_changed_or_invalid",
            "MP3 source changed since inspection".into(),
        )
    } else if duplicate {
        conflict(
            "duplicate_source_audio",
            "Select one MPEG source variant explicitly; no identity allocated".into(),
        )
    } else if exact.len() > 1 {
        conflict(
            "multiple_existing_audio_matches",
            "All matching MPEG files are preserved; selection is ambiguous".into(),
        )
    } else if exact.len() == 1 {
        Decision::Keep
    } else if retagged.len() > 1 {
        conflict(
            "multiple_existing_audio_matches",
            "Several MPEG tracks this tool inserted hold these frames; all are preserved".into(),
        )
    } else if let ([binding], [found], Some(desired)) = (&retagged[..], &matches[..], &desired) {
        // The cover is compared as sent: a PNG's or GIF's own bytes never
        // equal the JPEG that went with the track.
        match sent_cover(
            &sources.base_directory.join(&source.relative_path),
            &source.source,
        ) {
            Ok(cover) => retagged_mpeg_decision(
                binding,
                desired,
                found,
                cover,
                source.source.format().label(),
                catalog,
                index,
                journal,
                preserve_existing_tracks,
            ),
            Err(error) => conflict("source_changed_or_invalid", format!("{error:#}")),
        }
    } else if let ([binding], [found], Some(desired)) = (&converted[..], &matches[..], &desired) {
        // The cover that went with the ALAC: the FLAC's, as a JPEG.
        match sent_cover(
            &sources.base_directory.join(&source.relative_path),
            &source.source,
        ) {
            Ok(cover) => retagged_mpeg_decision(
                binding,
                desired,
                found,
                cover,
                "FLAC converted to ALAC",
                catalog,
                index,
                journal,
                preserve_existing_tracks,
            ),
            Err(error) => conflict("source_changed_or_invalid", format!("{error:#}")),
        }
    } else {
        conflict("mp3_insertion_unqualified", {
            let mut message = String::from(
                "No phone track holds this audio; no identity or destination is allocated.",
            );
            if let Some(report) = &preparation {
                for issue in &report.compatibility_issues {
                    message.push(' ');
                    message.push_str(&issue.detail);
                    message.push('.');
                }
            }
            message
        })
    };
    let cover = music_protocol::media::mpeg::cover_picture(&source.source.artwork);
    Ok(SourceDecision {
        relative_path: source.relative_path.clone(),
        managed_audio_replacement: None,
        source_file_bytes: Some(f.file_bytes),
        source_file_sha256: Some(f.full_file_sha256.clone()),
        encoded_audio_sha256: Some(f.audio.sha256.clone()),
        projected: desired.and_then(Result::ok),
        mp3: Some(source.source.clone()),
        mp3_insertion: preparation,
        flac_to_alac: false,
        validation_issues: music_protocol::media::mpeg::cover_left_out(&source.source.artwork)
            .map(|why| source_validation::Issue::cover_left_out(&why))
            .into_iter()
            .collect(),
        matches,
        artwork: if exact.is_empty() && retagged.is_empty() && converted.is_empty() {
            ArtworkDecision::UnavailableUntilSourceValidated
        } else {
            ArtworkDecision::PreservedExistingTrack {
                source_cover_sha256: cover.map(|c| c.sha256.clone()),
                source_cover_bytes: cover.map(|c| c.bytes),
                observed_cache_ids: exact
                    .iter()
                    .map(|t| t.sync_id)
                    .chain(
                        retagged
                            .iter()
                            .chain(&converted)
                            .map(|b| b.content.track_pid),
                    )
                    .filter_map(|id| catalog.artwork().track_cache_ids().get(&id).copied())
                    .collect(),
            }
        },
        native_composer: None,
        decision,
    })
}

/// The bindings of MP3s this tool inserted that hold `source`'s frames, each
/// with its file unchanged on the phone.
fn owned_mpeg_audio<'a>(
    source: &music_protocol::media::mpeg::Source,
    journal: &'a JournalView,
    index: &DeviceAudioIndex,
) -> Vec<&'a Binding> {
    let Ok(identity) = source.transferred_identity() else {
        return Vec::new();
    };
    journal
        .active_bindings
        .iter()
        .filter(|b| {
            !b.content.source.is_alac()
                && b.content.source.audio.sha256 == identity.audio.sha256
                && index.matches_transferred_source(
                    b.content.track_pid,
                    &b.content.path,
                    &b.content.source,
                )
        })
        .collect()
}

/// The bindings of ALACs this tool converted from `source`'s FLAC audio, each
/// with its file unchanged on the phone. The journal links one at most.
fn converted_flac_audio<'a>(
    source: &music_protocol::media::mpeg::Source,
    journal: &'a JournalView,
    index: &DeviceAudioIndex,
) -> Vec<&'a Binding> {
    let Ok(identity) = source.transferred_identity() else {
        return Vec::new();
    };
    journal
        .active_bindings
        .iter()
        .filter(|b| {
            b.content.source.is_alac()
                && journal
                    .conversions
                    .get(&b.content.track_pid)
                    .is_some_and(|c| c.source_audio == identity.audio.sha256)
                && index.matches_transferred_source(
                    b.content.track_pid,
                    &b.content.path,
                    &b.content.source,
                )
        })
        .collect()
}

/// An owned MP3 (or `label` track) whose source changed outside its frames:
/// Keep when the catalogue already shows its tags (or under add-only),
/// otherwise Update of the fields that differ. Its cover went with its audio
/// and is not sent on its own, so a source cover other than the one sent is a
/// conflict.
#[allow(clippy::too_many_arguments)]
fn retagged_mpeg_decision(
    binding: &Binding,
    desired: &Result<ProjectedFields>,
    found: &ExistingMatch,
    cover: Option<(String, usize)>,
    label: &str,
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    journal: &JournalView,
    preserve_existing_tracks: bool,
) -> Decision {
    if preserve_existing_tracks {
        return Decision::Keep;
    }
    let fields = match desired {
        Ok(fields) => fields,
        Err(error) => {
            return conflict(
                "metadata_update_unsupported",
                format!("the {label}'s tags are outside the insert-batch profile: {error:#}"),
            );
        }
    };
    let track = binding.content.track_pid;
    if !matches!(
        artwork_decision(
            cover.as_ref().map(|(sha, bytes)| (sha.as_str(), *bytes)),
            &[track],
            catalog,
            index,
            journal
        ),
        ArtworkDecision::VerifiedJournalMatch { .. }
            | ArtworkDecision::NoSourceCoverPreserveExisting
    ) {
        return conflict(
            "mp3_cover_update_unsupported",
            format!(
                "The source's front cover differs from the one sent with this {label}; its cover is sent only with its audio, so it is not updated"
            ),
        );
    }
    let Some(current) = catalog.tracks().iter().find(|t| t.sync_id == track) else {
        return conflict(
            "metadata_update_unsupported",
            "MPEG binding target absent".into(),
        );
    };
    let changes = &found.catalog_differences;
    match update::from_differences(fields, current, changes) {
        Ok(Some(_)) => Decision::Update {
            track_id: pid(track),
            changes: changes
                .iter()
                .map(|d| FieldDifference {
                    field: d.field,
                    current: d.current.clone(),
                    desired: d.desired.clone(),
                })
                .collect(),
        },
        Ok(None) => Decision::Keep,
        Err(error) => update_refused(&error),
    }
}

/// Why an owned track's changed tags are not sent as an edit. A field only a
/// replacement clears is offered to `sync --replace-changed`, which collects
/// these rows with the tracks whose audio changed.
fn update_refused(error: &anyhow::Error) -> Decision {
    match error.downcast_ref::<update::NeedsReplacement>() {
        Some(reason) => conflict(
            "metadata_update_needs_replacement",
            format!(
                "{reason}; sync with --replace-changed replaces the track (removes it, then adds the source again; its play and skip counts, ratings and playlist memberships are lost)"
            ),
        ),
        None => conflict("metadata_update_unsupported", format!("{error:#}")),
    }
}

fn artwork_recent_reset_disclosure(
    rows: &[SourceDecision],
    catalog: &Catalog,
) -> (Vec<Value>, Vec<Value>) {
    let proposed = rows.iter().any(|row| {
        !matches!(
            row.decision,
            Decision::Conflict { .. } | Decision::Skip { .. }
        ) && matches!(
            row.artwork,
            ArtworkDecision::PendingOwnedCover { .. }
                | ArtworkDecision::SeparateUpdateNeeded { .. }
        )
    });
    if !proposed {
        return (Vec::new(), Vec::new());
    }
    let mut tracks: Vec<_> = catalog.tracks().iter().collect();
    tracks.sort_by_key(|track| track.sync_id);
    let has_recent_skips = tracks
        .iter()
        .any(|track| track.playback.skip_count_recent > 0);
    let row = |track: &music_protocol::catalogue::sqlite::Track, before: u32| {
        if has_recent_skips {
            json!({
                "track_id":pid(track.sync_id),"before":before,"after":0,
                "condition":"artwork_import_may_reset_all_positive_recent_play_and_skip_counts",
                "all_statistics_except_recent_play_and_skip_must_be_preserved":true,
                "fresh_counts_rechecked_before_each_import":true
            })
        } else {
            // With no recent skips, only recent play counts can reset, so the
            // row can promise every other statistic is kept.
            json!({
                "track_id":pid(track.sync_id),"before":before,"after":0,
                "condition":"artwork_import_may_reset_all_positive_recent_play_counts",
                "lifetime_counts_and_all_other_statistics_must_be_preserved":true,
                "fresh_counts_rechecked_before_each_import":true
            })
        }
    };
    let plays = tracks
        .iter()
        .filter(|t| t.playback.play_count_recent > 0)
        .map(|t| row(t, t.playback.play_count_recent))
        .collect();
    let skips = tracks
        .iter()
        .filter(|t| t.playback.skip_count_recent > 0)
        .map(|t| row(t, t.playback.skip_count_recent))
        .collect();
    (plays, skips)
}

fn is_false(value: &bool) -> bool {
    !value
}

fn native_composer(catalog: &Catalog, name: Option<&str>) -> Result<Option<NativeComposer>> {
    let Some(name) = name else { return Ok(None) };
    let Some(existing) = catalog.composer_for_label(name)? else {
        return Ok(None);
    };
    let sort = existing
        .sort_name
        .as_deref()
        .context("existing composer has no qualified sort value")?;
    for text in [existing.name.as_str(), sort] {
        ensure!(
            !text.is_empty() && text.len() <= 4096 && !text.chars().any(char::is_control),
            "existing composer label or sort is outside the supported profile"
        );
    }
    Ok(Some(NativeComposer {
        requested_label: name.to_owned(),
        wire_label: existing.name.clone(),
        wire_sort: sort.to_owned(),
        existing_lookup_id: existing.internal_id,
    }))
}

fn source_row(source: &SourceTrack) -> SourceDecision {
    SourceDecision {
        relative_path: source.relative_path.clone(),
        managed_audio_replacement: None,
        source_file_bytes: Some(source.fingerprint.file_bytes),
        source_file_sha256: Some(hex(&source.fingerprint.full_file_sha256)),
        encoded_audio_sha256: Some(hex(&source.fingerprint.audio.sha256)),
        projected: None,
        mp3: None,
        mp3_insertion: None,
        flac_to_alac: false,
        validation_issues: Vec::new(),
        matches: Vec::new(),
        artwork: ArtworkDecision::UnavailableUntilSourceValidated,
        native_composer: None,
        decision: conflict("unvalidated", "Source has not been validated".into()),
    }
}

/// Selected sources no plan could add, whatever the phone holds, with why:
/// the source-side refusals of the planner above (inspection failure,
/// revalidation, projection or profile, duplicate audio, and an MP3 outside
/// the insert-batch profile), which an add-only plan skips. A whole-library
/// replacement checks this before it removes anything.
pub(crate) fn uninsertable_sources(sources: &SourceCollection) -> Vec<(String, String)> {
    let mut refused = Vec::new();
    let mut audio: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    for source in &sources.tracks {
        *audio.entry(source.fingerprint.audio.sha256).or_default() += 1;
    }
    for source in &sources.tracks {
        let report =
            source_validation::inspect(&sources.base_directory.join(&source.relative_path), false);
        let reason = if let Err(error) = check_revalidation(source, &report) {
            Some(format!("{error:#}"))
        } else if let Err(error) = ProjectedFields::from_source(&source.metadata) {
            Some(format!("{error:#}"))
        } else if !report.supported_by_current_profile {
            Some(
                report
                    .issues
                    .iter()
                    .filter(|issue| issue.severity == source_validation::Severity::Error)
                    .map(|issue| issue.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        } else if audio[&source.fingerprint.audio.sha256] > 1 {
            Some("another selected source has the same audio".into())
        } else {
            None
        };
        if let Some(reason) = reason {
            refused.push((source.relative_path.clone(), reason));
        }
    }
    for source in &sources.mp3_tracks {
        let path = sources.base_directory.join(&source.relative_path);
        let duplicate = sources
            .mp3_tracks
            .iter()
            .filter(|t| t.source.fingerprint.audio == source.source.fingerprint.audio)
            .count()
            > 1;
        let refusal = unconvertible(sources, &source.source)
            .or_else(|| {
                music_protocol::media::mpeg::read(&path)
                    .and_then(|fresh| {
                        ensure!(
                            fresh == source.source,
                            "MP3 source changed since inspection"
                        );
                        Ok(())
                    })
                    .err()
            })
            .or_else(|| {
                duplicate.then(|| anyhow::anyhow!("another selected source has the same audio"))
            })
            .or_else(|| {
                PreparedSource::read_expected(&path, &source.source)
                    .and_then(|prepared| prepared.ensure_batch_insertion_allowed())
                    .err()
            });
        if let Some(error) = refusal {
            refused.push((source.relative_path.clone(), format!("{error:#}")));
        }
    }
    for failed in &sources.failures {
        refused.push((failed.relative_path.clone(), failed.message.clone()));
    }
    refused.sort();
    refused
}

pub(crate) fn check_revalidation(
    source: &SourceTrack,
    report: &source_validation::Report,
) -> Result<()> {
    let audio = report
        .audio
        .as_ref()
        .context("source audio validation failed")?;
    ensure!(
        audio.file_bytes == source.fingerprint.file_bytes
            && audio.file_sha256 == hex(&source.fingerprint.full_file_sha256)
            && audio.encoded_audio_sha256 == hex(&source.fingerprint.audio.sha256)
            && audio.timescale == source.fingerprint.audio.timescale
            && audio.duration_ticks == source.fingerprint.audio.duration_ticks
            && audio.timescale == source.metadata.timing.timescale
            && audio.duration_ticks == source.metadata.timing.duration_ticks
            && audio.packets == source.metadata.timing.packets,
        "source fingerprint or exact timing changed after collection inspection"
    );
    // A caller cannot combine one file's fingerprints with another's projected tags.
    let tags = report
        .tags
        .as_ref()
        .context("source tags validation failed")?;
    let m = &source.metadata;
    ensure!(
        tags.title == m.title
            && tags.artist == m.artist
            && tags.album == m.album
            && tags.album_artist == m.album_artist
            && tags.genre == m.genre
            && tags.composer == m.composer
            && tags.copyright == m.copyright
            && tags.release_date == m.release_date
            && tags.compilation == m.compilation
            && tags.year == m.year
            && tags.content_rating == m.content_rating
            && tags.track == m.track
            && tags.track_total == m.track_total
            && tags.disc == m.disc
            && tags.disc_total == m.disc_total
            && tags.comment == m.comment
            && tags.sort_title == m.sort_title
            && tags.sort_artist == m.sort_artist
            && tags.sort_album == m.sort_album
            && tags.sort_album_artist == m.sort_album_artist
            && tags.sort_composer == m.sort_composer
            && audio.sample_rate_hz == m.sample_rate
            && audio.bit_depth == m.bit_depth
            && audio.channels == m.channels,
        "source metadata changed after collection inspection"
    );
    Ok(())
}

#[derive(Default)]
struct JournalView {
    active_bindings: Vec<Binding>,
    /// The FLAC each active converted track came from, by track.
    conversions: BTreeMap<u64, crate::engine::state::journal::Conversion>,
    reserved_ids: BTreeSet<u64>,
    reserved_paths: BTreeSet<String>,
    blockers: Vec<String>,
}
impl JournalView {
    fn read(
        evidence: Option<JournalEvidence<'_>>,
        identity: &IndexIdentity,
        index: &DeviceAudioIndex,
    ) -> Result<Self> {
        let Some(evidence) = evidence else {
            return Ok(Self::default());
        };
        ensure!(
            evidence.identity.device_digest() == identity.device_identifier_sha256
                && evidence.identity.library_digest() == identity.library_identity_sha256,
            "journal/index scope differs"
        );
        evidence.journal.ensure_identity(evidence.identity)?;
        let mut result = Self {
            active_bindings: evidence.journal.active_verified_bindings()?,
            conversions: evidence.journal.conversions()?,
            reserved_ids: evidence.journal.reserved_pids()?,
            reserved_paths: evidence.journal.reserved_paths()?,
            blockers: Vec::new(),
        };
        if let Some(attempt) = evidence.journal.outstanding()? {
            result.blockers.push(
                match attempt.state {
                    AttemptState::Prepared => "outstanding_prepared_requires_resolution",
                    AttemptState::ReconciliationRequired => {
                        "outstanding_started_requires_reconciliation"
                    }
                    _ => anyhow::bail!("unexpected closed journal attempt in outstanding state"),
                }
                .into(),
            );
        }
        for binding in &result.active_bindings {
            if !index.matches_transferred_source(
                binding.content.track_pid,
                &binding.content.path,
                &binding.content.source,
            ) {
                result.blockers.push(format!(
                    "verified_binding_drift:{}",
                    pid(binding.content.track_pid)
                ));
            }
        }
        Ok(result)
    }
    fn binding(&self, id: u64, index: &DeviceAudioIndex) -> Option<&Binding> {
        self.active_bindings.iter().find(|b| {
            b.content.track_pid == id
                && index.matches_transferred_source(id, &b.content.path, &b.content.source)
        })
    }
}

fn existing_match(
    source: &SourceTrack,
    track: &Track,
    index: &DeviceAudioIndex,
    journal: &JournalView,
    exact: bool,
    projected: Option<&ProjectedFields>,
    catalog: &Catalog,
) -> ExistingMatch {
    let (codec, full_sha) = if let Some(audio) = index.tracks().get(&track.sync_id) {
        ("alac", audio.fingerprint.full_file_sha256)
    } else {
        let audio = &index.other_audio()[&track.sync_id];
        (audio.codec.format().codec_label(), audio.full_file_sha256)
    };
    let ownership = if journal.binding(track.sync_id, index).is_some() {
        "inserted_by_us"
    } else {
        "unowned"
    };
    let (catalog_differences, canonicalized_fields) = projected
        .map(|p| compared_fields(p, catalog, track))
        .unwrap_or_default();
    ExistingMatch {
        track_id: pid(track.sync_id),
        wire_id: wire(track.sync_id),
        path: track.afc_path.clone().unwrap_or_default(),
        codec,
        encoded_audio_equal: exact,
        full_container_equal: full_sha == source.fingerprint.full_file_sha256,
        match_basis: None,
        ownership,
        catalog_differences,
        canonicalized_fields,
        device_file_sha256: None,
    }
}

/// The catalogue fields of `track` that differ from the source projection,
/// and those that agree only once the phone's composer lookup canonicalizes
/// the source's spelling.
fn compared_fields(
    projected: &ProjectedFields,
    catalog: &Catalog,
    track: &Track,
) -> (Vec<FieldDifference>, Vec<FieldDifference>) {
    let mut catalog_differences = differences(projected, track);
    catalog_differences.extend(source_metadata::additional_differences(
        projected, catalog, track,
    ));
    let mut canonicalized_fields = Vec::new();
    if let Some(name) = projected.composer.as_deref()
        && let Ok(Some(lookup)) = catalog.composer_for_label(name)
        && catalog
            .track_composers()
            .get(&track.sync_id)
            .and_then(|r| r.lookup_id)
            == Some(lookup.internal_id)
        && track.metadata.composer.as_deref() == Some(lookup.name.as_str())
        && let Some(index) = catalog_differences
            .iter()
            .position(|d| d.field == "composer")
    {
        canonicalized_fields.push(catalog_differences.remove(index));
    }
    (catalog_differences, canonicalized_fields)
}

/// The catalogue fields of `track` a source-driven Update would change.
pub(crate) fn catalogue_differences(
    projected: &ProjectedFields,
    catalog: &Catalog,
    track: &Track,
) -> Vec<FieldDifference> {
    compared_fields(projected, catalog, track).0
}

fn differences(s: &ProjectedFields, track: &Track) -> Vec<FieldDifference> {
    let m = &track.metadata;
    let mut output = Vec::new();
    let mut add = |field, current: Value, desired: Value, same: bool| {
        if !same {
            output.push(FieldDifference {
                field,
                current,
                desired,
            });
        }
    };
    for (field, current, desired, group) in [
        ("title", m.title.as_str(), s.title.as_str(), false),
        // The phone shows an item artist's one spelling on all its members.
        ("artist", &m.artist, &s.artist, true),
        ("album", &m.album, &s.album, true),
        ("album_artist", &m.album_artist, &s.grouping_artist, true),
    ] {
        add(
            field,
            json!(current),
            json!(desired),
            if group {
                same_name(current, desired)
            } else {
                current == desired
            },
        );
    }
    for (field, current, desired) in [
        ("genre", &m.genre, &s.genre),
        ("composer", &m.composer, &s.composer),
        ("copyright", &m.copyright, &s.copyright),
    ] {
        add(
            field,
            json!(current),
            json!(desired),
            current.as_deref().filter(|s| !s.is_empty())
                == desired.as_deref().filter(|s| !s.is_empty()),
        );
    }
    for (field, current, desired) in [
        ("year", u64::from(m.year), u64::from(s.year.unwrap_or(0))),
        ("track", m.track_number.into(), s.track.unwrap_or(0).into()),
        (
            "track_total",
            m.track_count.into(),
            s.track_total.unwrap_or(0).into(),
        ),
        // The phone stores disc 1 when an insert names none.
        ("disc", m.disc_number.into(), s.disc.unwrap_or(1).into()),
        (
            "disc_total",
            m.disc_count.into(),
            s.disc_total.unwrap_or(0).into(),
        ),
        ("total_time_ms", m.total_time_ms.into(), s.total_time_ms),
    ] {
        add(field, json!(current), json!(desired), current == desired);
    }
    output
}

/// A catalogue track an MP3 with no matching audio might be an earlier
/// version of, by the same rule as `plausible`.
fn plausible_mpeg(source: &PreparedSource, catalog: &Catalog) -> Option<String> {
    let assessment = source.assessment();
    let (metadata, timing) = (
        assessment.metadata.as_ref()?,
        assessment.coded_timing_proposal.as_ref()?,
    );
    catalog
        .tracks()
        .iter()
        .find(|track| {
            let m = &track.metadata;
            same_name(&metadata.title, &m.title)
                && (same_name(&metadata.artist, &m.artist) || same_name(&metadata.album, &m.album))
                && timing
                    .total_time_ms_floor
                    .abs_diff(u64::from(m.total_time_ms))
                    <= 1000
        })
        .map(|track| pid(track.sync_id))
}

fn plausible(source: &ProjectedFields, track: &Track) -> bool {
    let m = &track.metadata;
    same_name(&source.title, &m.title)
        && (same_name(&source.artist, &m.artist) || same_name(&source.album, &m.album))
        && source.total_time_ms.abs_diff(u64::from(m.total_time_ms)) <= 1000
}

/// A cover this tool still owes a track it inserted from this unchanged
/// source: none was sent yet (no record and no registration), or the phone
/// registered one whose image never arrived (`imageless_registration`: a
/// cover abandoned after the phone took its metadata).
fn pending_owned_cover(
    source: &music_protocol::media::fingerprint::SourceFingerprint,
    binding: &Binding,
    has_artwork_registration: bool,
    imageless_registration: bool,
    blocked: bool,
) -> bool {
    !blocked
        && binding.content.source == *source
        && (imageless_registration
            || (binding.content.artwork.is_none() && !has_artwork_registration))
}

/// `pending_owned_cover` for `track` as `catalog` registers it. Planning and
/// the cover step that finishes it decide the same way.
pub(crate) fn owes_cover(
    catalog: &Catalog,
    track: u64,
    source: &music_protocol::media::fingerprint::SourceFingerprint,
    binding: &Binding,
    blocked: bool,
) -> bool {
    let artwork = catalog.artwork();
    pending_owned_cover(
        source,
        binding,
        artwork.tracks_with_artwork_registration().contains(&track),
        artwork
            .track_cache_ids()
            .get(&track)
            .is_some_and(|id| !artwork.cached_ids().contains(id)),
        blocked,
    )
}

/// The SHA-256 and length of the cover an insert batch sends for a non-ALAC
/// source (`mp3::artwork::batch_cover`), read from the file.
fn sent_cover(
    path: &std::path::Path,
    source: &music_protocol::media::mpeg::Source,
) -> Result<Option<(String, usize)>> {
    Ok(
        crate::engine::planning::source_collection::mp3::artwork::batch_cover(path, source)?
            .map(|cover| (hex(&cover.sha256), cover.bytes.len())),
    )
}

fn artwork_decision(
    cover: Option<(&str, usize)>,
    matches: &[u64],
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    journal: &JournalView,
) -> ArtworkDecision {
    let Some((sha, bytes)) = cover else {
        return ArtworkDecision::NoSourceCoverPreserveExisting;
    };
    if matches.len() == 1
        && let Some(binding) = journal.binding(matches[0], index)
        && let Some(art) = &binding.content.artwork
        && hex(&art.sha256) == sha
        && art.bytes == bytes as u64
        && catalog.artwork().track_cache_ids().get(&matches[0]) == Some(&art.cache_id)
    {
        return ArtworkDecision::VerifiedJournalMatch {
            sha256: sha.into(),
            bytes,
            cache_id: art.cache_id,
        };
    }
    let observed: BTreeSet<_> = matches
        .iter()
        .filter_map(|id| catalog.artwork().track_cache_ids().get(id).copied())
        .collect();
    if !observed.is_empty() {
        ArtworkDecision::ExistingNeedsVerification {
            sha256: sha.into(),
            bytes,
            observed_cache_ids: observed.into_iter().collect(),
        }
    } else {
        ArtworkDecision::SeparateUpdateNeeded {
            sha256: sha.into(),
            bytes,
        }
    }
}

fn plan_add(
    planner: &mut Planner,
    source: &SourceTrack,
    candidate: &Candidate,
    folders: &BTreeSet<String>,
    paths: &mut BTreeSet<String>,
) -> Result<Decision> {
    candidate_path(&candidate.path, folders, paths)?;
    ensure!(
        source.fingerprint.file_bytes <= 64 * 1024 * 1024,
        "source exceeds production audio-transfer cap"
    );
    let planned = planner.plan(&source.metadata, candidate.ids)?;
    paths.insert(candidate.path.to_ascii_lowercase());
    Ok(Decision::Add {
        track_id: pid(planned.track_pid()),
        wire_id: wire(planned.track_pid()),
        album: group(planned.album_pid(), &planned),
        artist: group(planned.artist_pid(), &planned),
        path: candidate.path.clone(),
        pending_collection_ids: planned
            .pending_collection_ids()
            .iter()
            .map(|id| pid(*id))
            .collect(),
        fresh_state_required_before_execution: true,
    })
}
/// An MP3 addition, which goes in an insert batch. A FLAC that `converts` goes
/// as an ALAC (`.m4a`); its tags project as the ALAC's do, so it groups the
/// same.
fn plan_mpeg_add(
    planner: &mut Planner,
    source: &PreparedSource,
    converts: bool,
    candidate: &Candidate,
    folders: &BTreeSet<String>,
    paths: &mut BTreeSet<String>,
) -> Result<Decision> {
    source.ensure_batch_insertion_allowed()?;
    let format = if converts {
        music_protocol::media::format::Format::Alac
    } else {
        source.source().format()
    };
    candidate_path_for(&candidate.path, folders, paths, format.extension())?;
    let planned = planner.plan_mpeg(source, candidate.ids)?;
    paths.insert(candidate.path.to_ascii_lowercase());
    Ok(Decision::Add {
        track_id: pid(planned.track_pid()),
        wire_id: wire(planned.track_pid()),
        album: group(planned.album_pid(), &planned),
        artist: group(planned.artist_pid(), &planned),
        path: candidate.path.clone(),
        pending_collection_ids: planned
            .pending_collection_ids()
            .iter()
            .map(|id| pid(*id))
            .collect(),
        fresh_state_required_before_execution: true,
    })
}
fn group(id: u64, plan: &PlannedTrack) -> Group {
    Group {
        id: pid(id),
        wire_id: wire(id),
        status: if plan.new_collection_ids().contains(&id) {
            "new_this_source"
        } else if plan.pending_collection_ids().contains(&id) {
            "pending_earlier_source"
        } else {
            "reused_live"
        },
    }
}
fn candidate_path(
    path: &str,
    folders: &BTreeSet<String>,
    occupied: &BTreeSet<String>,
) -> Result<()> {
    candidate_path_for(path, folders, occupied, ".m4a")
}
/// A candidate destination: an `Fnn` folder the catalogue uses, four
/// uppercase letters and `extension` (its dot included), unused.
fn candidate_path_for(
    path: &str,
    folders: &BTreeSet<String>,
    occupied: &BTreeSet<String>,
    extension: &str,
) -> Result<()> {
    let (folder, name) = path
        .rsplit_once('/')
        .context("candidate path lacks folder")?;
    let suffix = folder
        .strip_prefix("iTunes_Control/Music/F")
        .context("candidate path outside Music")?;
    ensure!(
        suffix.len() == 2 && suffix.bytes().all(|b| b.is_ascii_digit()) && folders.contains(folder),
        "candidate requires a folder represented by the current catalog"
    );
    ensure!(
        name.len() == 4 + extension.len()
            && name.ends_with(extension)
            && name.as_bytes()[..4].iter().all(u8::is_ascii_uppercase),
        "candidate requires four uppercase basename characters and {extension}"
    );
    ensure!(
        !occupied.contains(&path.to_ascii_lowercase()),
        "candidate destination is referenced or historically reserved"
    );
    Ok(())
}
fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && !path.starts_with('/')
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains(['\\', '\0'])),
        "invalid relative source path"
    );
    Ok(())
}
fn same_name(a: &str, b: &str) -> bool {
    a == b || (a.is_ascii() && b.is_ascii() && a.eq_ignore_ascii_case(b))
}
/// Why `--flac-to-alac` leaves out `source`: a FLAC that cannot be converted
/// without losing audio.
pub(crate) fn unconvertible(
    sources: &SourceCollection,
    source: &music_protocol::media::mpeg::Source,
) -> Option<anyhow::Error> {
    (sources.flac_to_alac && source.format() == music_protocol::media::format::Format::Flac)
        .then(|| crate::engine::execution::flac_to_alac::convertible(source).err())
        .flatten()
}

fn conflict(code: &'static str, message: String) -> Decision {
    Decision::Conflict { code, message }
}
fn pid(value: u64) -> String {
    format!("{value:016x}")
}
fn wire(value: u64) -> String {
    (value as i64).to_string()
}
fn hex(value: &[u8; 32]) -> String {
    value.iter().map(|b| format!("{b:02x}")).collect()
}
