//! Removal selection: which catalogue rows a delete, prune or Clear request
//! selects, from previously captured state.
//!
//! The plan decides what is selected; it does not decide whether the phone can
//! remove it. Execution checks every selected row against the removal model
//! (`verification::removal_sql`) on the same snapshot, then again on a fresh
//! capture before each batch, and verifies each batch after the phone applies
//! it. Rows outside the observed receiver rules (placeholders, unsupported
//! types, rows without a path) stay blocked here.
//! Prune is deliberately limited to the exact original full-directory scope:
//! the journal retains its digest but not each original source relative path.
//! Sync and `plan --remove-missing` select by the absolute source path the
//! journal pins for each track this tool inserted (`Origin`), under any
//! selection.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    engine::planning::additive::{ExcludedSource, JournalEvidence, SelectionScope},
    engine::planning::source_collection::{ExclusionReason, SourceCollection, SourceTrack},
    engine::state::audio_index::{DeviceAudioIndex, IndexIdentity},
    engine::state::journal::{AttemptState, Binding, Conversion},
};
use music_protocol::catalogue::sqlite::{Catalog, Track};
use music_protocol::media::fingerprint::TransferredSource;

const MAX_TRACKS: usize = 50_000;
const MAX_SOURCES: usize = 4096;

pub enum Request<'a> {
    Prune {
        sources: &'a SourceCollection,
    },
    /// The tracks this tool inserted whose source file is gone from the
    /// selection. `sources` (the inspected selection) is needed only when an
    /// origin is `Missing`: a file moved within the selection keeps its track.
    Missing {
        sources: Option<&'a SourceCollection>,
        origins: &'a BTreeMap<u64, Origin>,
    },
    Delete {
        track_ids: &'a BTreeSet<u64>,
    },
    Clear,
}

/// Where a track this tool inserted came from, as the journal pins it,
/// against the current selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Its source path is one the selection reaches, and nothing is there.
    Missing(PathBuf),
    /// A file is still at its source path.
    Present,
    /// The selection does not reach its source path.
    Outside,
    /// The journal pins no usable source path; the reason.
    Unknown(String),
}

#[derive(Serialize)]
pub struct Plan {
    pub schema: &'static str,
    pub mode: &'static str,
    pub device_sha256: String,
    pub library_sha256: String,
    pub database_sha256: String,
    pub database_bytes: u64,
    pub wal_sha256: Option<String>,
    pub wal_bytes: Option<u64>,
    pub media_anchor: u32,
    pub catalog_track_count: usize,
    pub complete_audio_index_verified: bool,
    pub source: Option<SourceScope>,
    pub tracks: Vec<TrackDecision>,
    pub proposed_removal_ids: Vec<String>,
    /// Blocked rows are preserved too; omission is never a removal request.
    pub preserved_ids: Vec<String>,
    pub affected_audio_bytes: u64,
    /// Some only for Clear. False if even one row is unavailable/unsupported or
    /// a global guard fails. This is never a cloud-account deletion claim.
    pub entire_catalog_proposed: Option<bool>,
    pub blockers: Vec<String>,
}

impl Plan {
    pub fn has_blockers(&self) -> bool {
        !self.blockers.is_empty() || self.tracks.iter().any(|r| r.decision == Decision::Blocked)
    }
    /// Whether only the selection's unreadable sources block it: a file that
    /// could not be inspected, or an MP3 whose frames could not be
    /// identified, may be a moved source of a track it would remove, so
    /// every removal waits until that file reads (`sync`'s removals).
    pub fn held_by_unreadable_sources(&self) -> bool {
        const HELD: [&str; 2] = [
            "source_inspection_incomplete",
            "source_mpeg_identity_invalid",
        ];
        !self.blockers.is_empty()
            && self.blockers.iter().all(|b| HELD.contains(&b.as_str()))
            && self
                .tracks
                .iter()
                .filter(|row| row.decision == Decision::Blocked)
                .all(|row| {
                    row.reasons.iter().all(|reason| {
                        matches!(
                            *reason,
                            "global_precondition_failed"
                                | "source_mpeg_identity_invalid"
                                | "audio_file_already_gone"
                        )
                    })
                })
    }
}

#[derive(Serialize)]
pub struct SourceScope {
    pub root: String,
    pub collection_sha256: String,
    pub selection: SelectionScope,
    pub exclusions: Vec<ExcludedSource>,
    pub sources: Vec<SourceEvidence>,
}

#[derive(Serialize)]
pub struct SourceEvidence {
    pub relative_path: String,
    pub file_bytes: u64,
    pub full_file_sha256: String,
    pub encoded_audio_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<&'static str>,
}

#[derive(Serialize)]
pub struct TrackDecision {
    pub track_id: String,
    pub wire_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: Option<String>,
    pub file_bytes: u64,
    pub codec: Option<&'static str>,
    pub full_file_sha256: Option<String>,
    pub encoded_audio_sha256: Option<String>,
    pub journal_origin: &'static str,
    pub journal_binding_matches_audio: bool,
    pub journal_collection_sha256: Option<String>,
    pub requested: bool,
    pub decision: Decision,
    pub reasons: Vec<&'static str>,
    pub matching_source_paths: Vec<String>,
    /// The source path the journal pins, where no file is left
    /// (sync and `plan --remove-missing` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_source_path: Option<String>,
    pub unrelated_track_acknowledgement_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    ProposedRemoval,
    Preserved,
    Blocked,
}

/// All inputs are previously captured/inspected values. Only an optional local
/// journal is read; no connection is discovered and no file is created.
/// Namespace or exact catalog-byte mismatch returns Err before any plan output.
/// Scope/availability/coverage findings produce a blocked, inspectable plan.
pub fn build(
    catalog: &Catalog,
    index: &DeviceAudioIndex,
    identity: &IndexIdentity,
    journal: Option<JournalEvidence<'_>>,
    request: Request<'_>,
) -> Result<Plan> {
    ensure!(
        catalog.tracks().len() <= MAX_TRACKS,
        "catalog exceeds planning bound"
    );
    ensure!(index.identity() == identity, "audio index identity differs");
    let h = catalog.source_hashes();
    let bound = index.catalog();
    ensure!(
        h.database == bound.database_sha256
            && h.database_bytes == bound.database_bytes
            && h.wal == bound.wal_sha256
            && h.wal_bytes == bound.wal_bytes
            && catalog.persisted_media_anchor() == bound.persisted_media_anchor,
        "audio index catalog bytes or anchor differ"
    );
    let by_id: BTreeMap<_, _> = catalog.tracks().iter().map(|t| (t.sync_id, t)).collect();
    ensure!(
        by_id.len() == catalog.tracks().len() && !by_id.contains_key(&0),
        "catalog track identities are zero or ambiguous"
    );
    let mut blockers = Vec::new();
    let complete_index = index.ensure_matches(catalog, identity).is_ok();
    if !complete_index {
        blockers.push("audio_index_incomplete_or_unsupported_catalog".into());
    }
    let journal_present = journal.is_some();
    let conversions = journal
        .map(|evidence| evidence.journal.conversions())
        .transpose()?
        .unwrap_or_default();
    let bindings = active_journal_bindings(journal, identity, index, &mut blockers)?;
    let (mode, source, inspected) = match &request {
        Request::Prune { sources } => {
            let evidence = source_scope(sources, &mut blockers)?;
            if !journal_present {
                blockers.push("prune_requires_existing_journal".into());
            }
            ("prune", Some(evidence), Some(*sources))
        }
        Request::Missing { sources, origins } => {
            if origins.values().any(|o| matches!(o, Origin::Missing(_))) {
                match sources {
                    None => blockers.push("source_inspection_absent".into()),
                    Some(sources) => {
                        // A file that cannot be read could be a moved source.
                        if !sources.failures.is_empty() {
                            blockers.push("source_inspection_incomplete".into());
                        }
                        // An unmounted or emptied directory proves nothing.
                        if sources.selection_stats.selected_files == 0 {
                            blockers.push("empty_source_cannot_authorize_removal".into());
                        }
                    }
                }
            }
            ("remove_missing", None, *sources)
        }
        Request::Delete { track_ids } => {
            ensure!(
                !track_ids.is_empty(),
                "explicit deletion selection is empty"
            );
            ensure!(
                track_ids.len() <= MAX_TRACKS,
                "deletion selection exceeds bound"
            );
            ensure!(
                track_ids.iter().all(|id| by_id.contains_key(id)),
                "explicit deletion ID is absent from catalog"
            );
            ("delete", None, None)
        }
        Request::Clear => ("clear", None, None),
    };
    let scope = match request {
        Request::Prune { sources } => Some(collection_scope_sha256(sources)?),
        _ => None,
    };
    // Validate each MPEG descriptor before any row can infer source absence.
    // Retain the checked frame identity independently of its ID3 file layout.
    let mpeg_sources = inspected.and_then(|sources| {
        match sources
            .mp3_tracks
            .iter()
            .map(|source| source.source.transferred_identity())
            .collect::<Result<Vec<_>>>()
        {
            Ok(identities) => Some(identities),
            Err(_) => {
                blockers.push("source_mpeg_identity_invalid".into());
                None
            }
        }
    });
    let mut tracks = Vec::with_capacity(by_id.len());
    for track in by_id.values() {
        tracks.push(decide_track(
            track,
            index,
            bindings.get(&track.sync_id),
            conversions.get(&track.sync_id),
            &request,
            scope,
            mpeg_sources.as_deref(),
            !blockers.is_empty(),
        ));
    }
    finish(
        catalog,
        identity,
        mode,
        source,
        complete_index,
        blockers,
        tracks,
    )
}

#[allow(clippy::too_many_arguments)]
fn decide_track(
    track: &Track,
    index: &DeviceAudioIndex,
    binding: Option<&Binding>,
    conversion: Option<&Conversion>,
    request: &Request<'_>,
    scope: Option<[u8; 32]>,
    mpeg_sources: Option<&[TransferredSource]>,
    global_blocked: bool,
) -> TrackDecision {
    let mut row = track_row(track, index, binding);
    let mut reasons = availability_reasons(track);
    row.requested = match request {
        Request::Clear => true,
        Request::Delete { track_ids } => track_ids.contains(&track.sync_id),
        Request::Prune { sources } => prune_target(
            track,
            binding,
            conversion,
            sources,
            scope,
            mpeg_sources,
            &mut row,
            &mut reasons,
        ),
        Request::Missing { sources, origins } => missing_target(
            track,
            binding,
            conversion,
            *sources,
            origins,
            mpeg_sources,
            &mut row,
            &mut reasons,
        ),
    };
    if row.requested {
        row.unrelated_track_acknowledgement_required =
            !(binding.is_some() && row.journal_binding_matches_audio);
        if global_blocked {
            reasons.push("global_precondition_failed");
        }
        row.decision = if reasons.is_empty() {
            Decision::ProposedRemoval
        } else {
            Decision::Blocked
        };
    }
    // Retain availability findings even when this row was not selected.
    row.reasons.extend(reasons);
    if index.gone().contains(&track.sync_id) {
        row.reasons.push("audio_file_already_gone");
    }
    if !row.requested && row.reasons.is_empty() {
        row.reasons.push("not_selected");
    }
    row
}

fn finish(
    catalog: &Catalog,
    identity: &IndexIdentity,
    mode: &'static str,
    source: Option<SourceScope>,
    complete_audio_index_verified: bool,
    blockers: Vec<String>,
    tracks: Vec<TrackDecision>,
) -> Result<Plan> {
    let proposed_removal_ids: Vec<_> = tracks
        .iter()
        .filter(|r| r.decision == Decision::ProposedRemoval)
        .map(|r| r.track_id.clone())
        .collect();
    let preserved_ids = tracks
        .iter()
        .filter(|r| r.decision != Decision::ProposedRemoval)
        .map(|r| r.track_id.clone())
        .collect();
    let affected_audio_bytes = tracks
        .iter()
        .filter(|r| r.decision == Decision::ProposedRemoval)
        .try_fold(0_u64, |n, r| {
            n.checked_add(r.file_bytes)
                .context("affected audio byte count overflow")
        })?;
    let entire_catalog_proposed = (mode == "clear")
        .then_some(blockers.is_empty() && proposed_removal_ids.len() == tracks.len());
    let hashes = catalog.source_hashes();
    Ok(Plan {
        schema: "rustunes.destructive-plan",
        mode,
        device_sha256: hex(&identity.device_identifier_sha256),
        library_sha256: hex(&identity.library_identity_sha256),
        database_sha256: hex(&hashes.database),
        database_bytes: hashes.database_bytes,
        wal_sha256: hashes.wal.as_ref().map(hex),
        wal_bytes: hashes.wal_bytes,
        media_anchor: catalog.persisted_media_anchor(),
        catalog_track_count: tracks.len(),
        complete_audio_index_verified,
        source,
        tracks,
        proposed_removal_ids,
        preserved_ids,
        affected_audio_bytes,
        entire_catalog_proposed,
        blockers,
    })
}

fn active_journal_bindings(
    journal: Option<JournalEvidence<'_>>,
    identity: &IndexIdentity,
    index: &DeviceAudioIndex,
    blockers: &mut Vec<String>,
) -> Result<BTreeMap<u64, Binding>> {
    let Some(evidence) = journal else {
        return Ok(BTreeMap::new());
    };
    ensure!(
        evidence.identity.device_digest() == identity.device_identifier_sha256
            && evidence.identity.library_digest() == identity.library_identity_sha256,
        "journal/index identity differs"
    );
    evidence.journal.ensure_identity(evidence.identity)?;
    if let Some(attempt) = evidence.journal.outstanding()? {
        blockers.push(
            match attempt.state {
                AttemptState::Prepared => "outstanding_prepared_requires_resolution",
                AttemptState::ReconciliationRequired => {
                    "outstanding_started_requires_reconciliation"
                }
                _ => anyhow::bail!("unexpected closed outstanding attempt"),
            }
            .into(),
        );
    }
    let mut output = BTreeMap::new();
    for binding in evidence.journal.active_verified_bindings()? {
        // A file an interrupted removal took (`DeviceAudioIndex::gone`) is
        // not drift: the caller accepted it, and only removing the row helps.
        if !binding_matches(&binding, index) && !index.gone().contains(&binding.content.track_pid) {
            blockers.push(format!(
                "journal_binding_drift:{:016x}",
                binding.content.track_pid
            ));
        }
        ensure!(
            output.insert(binding.content.track_pid, binding).is_none(),
            "duplicate journal track binding"
        );
    }
    Ok(output)
}

fn binding_matches(binding: &Binding, index: &DeviceAudioIndex) -> bool {
    index.matches_transferred_source(
        binding.content.track_pid,
        &binding.content.path,
        &binding.content.source,
    )
}

fn track_row(track: &Track, index: &DeviceAudioIndex, binding: Option<&Binding>) -> TrackDecision {
    let alac = index.tracks().get(&track.sync_id);
    let other = index.other_audio().get(&track.sync_id);
    TrackDecision {
        track_id: format!("{:016x}", track.sync_id),
        wire_id: (track.sync_id as i64).to_string(),
        title: track.metadata.title.clone(),
        artist: track.metadata.artist.clone(),
        album: track.metadata.album.clone(),
        path: track.afc_path.clone(),
        file_bytes: track.metadata.file_size,
        codec: alac
            .map(|_| "alac")
            .or(other.map(|a| a.codec.format().codec_label())),
        full_file_sha256: alac
            .map(|a| hex(&a.fingerprint.full_file_sha256))
            .or_else(|| other.map(|a| hex(&a.full_file_sha256))),
        encoded_audio_sha256: alac.map(|a| hex(&a.fingerprint.audio.sha256)),
        journal_origin: if binding.is_some() {
            "inserted_by_us"
        } else {
            "unowned"
        },
        journal_binding_matches_audio: binding.is_some_and(|b| binding_matches(b, index)),
        journal_collection_sha256: binding.and_then(|b| b.content.collection.as_ref()).map(hex),
        requested: false,
        decision: Decision::Preserved,
        reasons: Vec::new(),
        matching_source_paths: Vec::new(),
        missing_source_path: None,
        unrelated_track_acknowledgement_required: false,
    }
}

fn availability_reasons(track: &Track) -> Vec<&'static str> {
    let a = &track.availability;
    let mut reasons = Vec::new();
    if a.is_placeholder || !a.is_library_added || !a.has_non_purgeable_asset {
        reasons.push("track_unavailable");
    }
    if !a.is_supported_type {
        reasons.push("track_type_unsupported");
    }
    if track.afc_path.is_none() {
        reasons.push("track_permanent_path_absent");
    }
    reasons
}

#[allow(clippy::too_many_arguments)]
fn prune_target(
    track: &Track,
    binding: Option<&Binding>,
    conversion: Option<&Conversion>,
    sources: &SourceCollection,
    scope: Option<[u8; 32]>,
    mpeg_sources: Option<&[TransferredSource]>,
    row: &mut TrackDecision,
    reasons: &mut Vec<&'static str>,
) -> bool {
    let Some(binding) = binding else {
        row.reasons.push("unowned_track_preserved");
        return false;
    };
    if binding.content.collection.is_none() || binding.content.collection != scope {
        row.reasons.push("different_or_unknown_source_scope");
        return false;
    }
    let Some(mpeg_sources) = mpeg_sources else {
        row.reasons.push("source_mpeg_identity_invalid");
        return false;
    };
    row.matching_source_paths = present_audio(binding, conversion, sources, mpeg_sources);
    if !row.matching_source_paths.is_empty() {
        row.reasons.push("encoded_audio_present_in_source");
        return false;
    }
    if sources.tracks.iter().any(|source| plausible(source, track))
        || sources.mp3_tracks.iter().any(|source| {
            let m = &source.source.metadata;
            let name = |values: &[String], stored: &str| {
                values.iter().any(|v| {
                    v == stored
                        || (v.is_ascii() && stored.is_ascii() && v.eq_ignore_ascii_case(stored))
                })
            };
            // Coded MPEG duration is only a conservative ambiguity signal here,
            // never decoded equivalence or proof of source absence.
            let audio = &source.source.fingerprint.audio;
            let ms = audio.coded_samples * 1000 / u64::from(audio.sample_rate);
            name(&m.title, &track.metadata.title)
                && (name(&m.artist, &track.metadata.artist)
                    || name(&m.album, &track.metadata.album))
                && ms.abs_diff(u64::from(track.metadata.total_time_ms)) <= 1000
        })
    {
        reasons.push("possible_reencoded_source_requires_comparison");
    }
    true
}

/// The sources holding the encoded audio this tool sent for `binding`, or
/// the FLAC audio its ALAC was converted from (`conversion`).
fn present_audio(
    binding: &Binding,
    conversion: Option<&Conversion>,
    sources: &SourceCollection,
    mpeg_sources: &[TransferredSource],
) -> Vec<String> {
    let mut paths = Vec::new();
    for source in &sources.tracks {
        if source.fingerprint.audio == binding.content.source.audio {
            paths.push(source.relative_path.clone());
        }
    }
    for (source, identity) in sources.mp3_tracks.iter().zip(mpeg_sources) {
        // The checked MPEG digest commits rate/channels/frame count/audio bytes
        // and frame hash, but not leading/trailing metadata layout. Full
        // transferred equality still guards the journal-to-device binding.
        if (!binding.content.source.is_alac()
            && identity.audio.sha256 == binding.content.source.audio.sha256)
            || conversion.is_some_and(|c| identity.audio.sha256 == c.source_audio)
        {
            paths.push(source.relative_path.clone());
        }
    }
    paths
}

/// A track this tool inserted is removed when the selection reaches its
/// source path and no file is there, unless its encoded audio is still in the
/// selection under another path: a moved file keeps its track, which the
/// sync matches there. Unowned and unattributed tracks stay.
#[allow(clippy::too_many_arguments)]
fn missing_target(
    track: &Track,
    binding: Option<&Binding>,
    conversion: Option<&Conversion>,
    sources: Option<&SourceCollection>,
    origins: &BTreeMap<u64, Origin>,
    mpeg_sources: Option<&[TransferredSource]>,
    row: &mut TrackDecision,
    reasons: &mut Vec<&'static str>,
) -> bool {
    let Some(binding) = binding else {
        row.reasons.push("unowned_track_preserved");
        return false;
    };
    let path = match origins.get(&track.sync_id) {
        Some(Origin::Missing(path)) => path,
        Some(Origin::Present) => {
            row.reasons.push("source_present");
            return false;
        }
        Some(Origin::Outside) => {
            row.reasons.push("source_outside_selection");
            return false;
        }
        Some(Origin::Unknown(_)) | None => {
            row.reasons.push("source_path_unknown");
            return false;
        }
    };
    row.missing_source_path = Some(path.display().to_string());
    match (sources, mpeg_sources) {
        (Some(sources), Some(mpeg_sources)) => {
            row.matching_source_paths = present_audio(binding, conversion, sources, mpeg_sources);
            if !row.matching_source_paths.is_empty() {
                row.reasons.push("encoded_audio_present_in_source");
                return false;
            }
        }
        (Some(_), None) => reasons.push("source_mpeg_identity_invalid"),
        (None, _) => reasons.push("source_inspection_absent"),
    }
    true
}

fn plausible(source: &SourceTrack, track: &Track) -> bool {
    let m = &source.metadata;
    let name = |values: &[String], stored: &str| {
        values.iter().any(|value| {
            value == stored
                || (value.is_ascii() && stored.is_ascii() && value.eq_ignore_ascii_case(stored))
        })
    };
    let duration = source
        .fingerprint
        .audio
        .duration_ticks
        .checked_mul(1000)
        .and_then(|n| n.checked_div(u64::from(source.fingerprint.audio.timescale)));
    name(&m.title, &track.metadata.title)
        && (name(&m.artist, &track.metadata.artist) || name(&m.album, &track.metadata.album))
        && duration.is_some_and(|n| n.abs_diff(u64::from(track.metadata.total_time_ms)) <= 1000)
}

/// The scope digest each binding records and prune matches. Changing this
/// encoding leaves existing bindings out of prune until a `pull` rebinds them.
pub fn collection_scope_sha256(sources: &SourceCollection) -> Result<[u8; 32]> {
    let root = sources.root.to_str().context("source root is not UTF-8")?;
    Ok(Sha256::digest(serde_json::to_vec(&serde_json::json!({"root":root,"recursive":sources.options.recursive,"include":sources.options.include,"exclude":sources.options.exclude}))?).into())
}

fn source_scope(sources: &SourceCollection, blockers: &mut Vec<String>) -> Result<SourceScope> {
    ensure!(
        sources.inspected_len() <= MAX_SOURCES && sources.exclusions.len() <= 10_000,
        "source collection exceeds planning bound"
    );
    if sources.ensure_unambiguous().is_err() {
        blockers.push("source_inspection_incomplete_or_ambiguous".into());
    }
    if sources.inspected_len() == 0 {
        blockers.push("empty_source_cannot_authorize_prune".into());
    }
    if !sources.root.is_absolute() || sources.root != sources.base_directory {
        blockers.push("prune_requires_full_directory_scope".into());
    }
    if !sources.options.include.is_empty() || !sources.options.exclude.is_empty() {
        blockers.push("filtered_scope_cannot_authorize_prune".into());
    }
    let bytes = sources.inspected_bytes()?;
    if sources.selection_stats.selected_files != sources.inspected_len()
        || sources.selection_stats.selected_bytes != bytes
    {
        blockers.push("source_selection_coverage_mismatch".into());
    }
    // Without recursion the subfolders are left out; only tracks sent the
    // same way share this scope.
    if sources.exclusions.iter().any(|e| match e.reason {
        ExclusionReason::HiddenDirectory | ExclusionReason::UnsupportedExtension => false,
        ExclusionReason::NonRecursive => sources.options.recursive,
        _ => true,
    }) {
        blockers.push("unsafe_or_narrowed_source_exclusions".into());
    }
    let mut seen_paths = BTreeSet::new();
    let mut seen_audio = BTreeSet::new();
    if sources.tracks.iter().any(|t| {
        !seen_paths.insert(&t.relative_path) || !seen_audio.insert(t.fingerprint.audio.sha256)
    }) {
        blockers.push("source_aliases_or_duplicates_require_review".into());
    }
    let mut source_rows: Vec<_> = sources
        .tracks
        .iter()
        .map(|t| SourceEvidence {
            relative_path: t.relative_path.clone(),
            file_bytes: t.fingerprint.file_bytes,
            full_file_sha256: hex(&t.fingerprint.full_file_sha256),
            encoded_audio_sha256: hex(&t.fingerprint.audio.sha256),
            codec: None,
        })
        .collect();
    for t in &sources.mp3_tracks {
        if !seen_paths.insert(&t.relative_path) {
            blockers.push("source_aliases_or_duplicates_require_review".into());
        }
        source_rows.push(SourceEvidence {
            relative_path: t.relative_path.clone(),
            file_bytes: t.source.fingerprint.file_bytes,
            full_file_sha256: t.source.fingerprint.full_file_sha256.clone(),
            encoded_audio_sha256: t.source.fingerprint.audio.sha256.clone(),
            codec: Some("mpeg1-layer3"),
        });
    }
    source_rows.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(SourceScope {
        root: sources
            .root
            .to_str()
            .context("source root is not UTF-8")?
            .into(),
        collection_sha256: hex(&collection_scope_sha256(sources)?),
        selection: SelectionScope {
            base_directory: sources
                .base_directory
                .to_str()
                .context("source base directory is not UTF-8")?
                .into(),
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
        sources: source_rows,
    })
}
fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
