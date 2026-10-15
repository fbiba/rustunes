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

#[derive(Clone, Copy, Default)]
pub struct DecisionCounts {
    pub additions: usize,
    pub matched: usize,
    pub updates: usize,
    pub conflicts: usize,
    pub skipped: usize,
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
