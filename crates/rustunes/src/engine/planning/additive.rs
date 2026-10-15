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
