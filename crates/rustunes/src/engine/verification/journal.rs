//! Bounded read-only checks of active verified journal bindings only.
//! Retired lifetimes remain historical evidence and are never scheduled here.
//!
//! No ATC, journal transitions or phone writes. Full audio
//! equality covers the transferred container (including its embedded tags), not
//! a desired source revision or the independent SQL metadata projection. Artwork
//! originals are checked against completed per-track cover bindings, and their
//! derivatives are decoded. Shared album representatives are informational.
//! Per-file stable reads are not an atomic library snapshot.

use crate::engine::execution::device::DeviceProvider;
use crate::{
    engine::execution::native_insert::{FreshState, create_trace, durable_write},
    engine::state::current::Current,
    engine::state::journal::{Binding, Identity, Journal},
    engine::verification::artwork::{self, TrackArtworkExpectation, VerifiedTrackArtworkBytes},
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::{Catalog, Track};
use music_protocol::protocol::transfer::{
    Attributes, MAX_AUDIO, MAX_BOUNDED_OPS, Transfer, hex, regular_size,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    future::Future,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

mod mpeg;
pub use mpeg::MpegFrameReport;

const MAX_BINDINGS: usize = 4096;
const MAX_TOTAL: u64 = 64 * 1024 * 1024 * 1024;
const CHUNK: u64 = 64 * 1024;
/// Bytes per AFC read request: an audio read costs one operation per request.
const REQUEST: u64 = music_protocol::protocol::afc::copy::CHUNK as u64;
const MAX_DURATION: Duration = Duration::from_secs(1800);
const MAX_ATTRIBUTES_BYTES: usize = 16 * 1024;

pub enum Selection {
    All,
    TrackIds(BTreeSet<u64>),
}
pub struct ReadControl {
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}
impl ReadControl {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            "journal verification cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "original journal verification deadline expired"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioStatus {
    NotRead,
    CatalogMismatch,
    MissingFile,
    AttributesMismatch,
    ReadPendingStateGuard,
    VerifiedTransferredContainer,
    HashMismatch,
    EncodedIdentityMismatch,
}

#[derive(Serialize)]
pub struct ArtworkStatus {
    pub expected_cache_id: Option<u32>,
    pub expected_source_cover_sha256: Option<String>,
    pub observed_track_cache_id: Option<u32>,
    pub observed_album_cache_id: Option<u32>,
    /// Expected source300 track registration plus cached SQL row, not pixels.
    pub track_registration_matches: Option<bool>,
    pub album_registration: &'static str,
    pub registration: &'static str,
    pub permanent_cache_verification: &'static str,
    pub permanent_cache_proof: Option<VerifiedTrackArtworkBytes>,
    pub permanent_cache_failure: Option<String>,
}

#[derive(Serialize)]
pub struct TrackReport {
    pub track_id: String,
    pub wire_track_id: String,
    pub path: String,
    pub audio_bytes: u64,
    pub expected_audio_sha256: String,
    pub encoded_audio_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpeg_frame_readback: Option<MpegFrameReport>,
    /// Set when the file's stat matched one whose SHA-256 a verified copy or
    /// this host's upload recorded, instead of copying the file again; absent
    /// for a readback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_proof: Option<&'static str>,
    pub audio_status: AudioStatus,
    pub audio_observed_sha256: Option<String>,
    pub audio_attributes: Option<Attributes>,
    pub finding: Option<&'static str>,
    pub batch: Option<usize>,
    pub artwork: ArtworkStatus,
}

#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub status: &'static str,
    pub verification_completed: bool,
    pub all_selected_audio_verified: bool,
    pub device_identifier_sha256: String,
    pub library_identity_sha256: String,
    pub input_receipt_sha256: String,
    pub media_anchor: u32,
    pub selected_bindings: usize,
    /// Active verified bindings only.
    pub total_verified_journal_bindings: usize,
    pub outstanding_attempt_present: bool,
    pub planned_audio_bytes: u64,
    /// Includes a conservative rounded upper bound for every state guard.
    pub planned_total_read_bytes_upper_bound: u64,
    pub planned_batches: usize,
    pub completed_batches: usize,
    pub planned_artwork_tracks: usize,
    pub completed_artwork_tracks: usize,
    pub planned_artwork_read_bytes_upper_bound: u64,
    pub tracks: Vec<TrackReport>,
    pub failure: Option<String>,
    pub phone_writes: u32,
    pub journal_mutations: u32,
    /// Always false: SQL metadata is not compared (`limitations`).
    pub metadata_projection_verified: bool,
    pub permanent_artwork_verified: bool,
    /// Always false: the reads are no atomic snapshot (`limitations`).
    pub atomic_snapshot_claimed: bool,
    pub limitations: [&'static str; 4],
}

struct Batch {
    indices: Vec<usize>,
    operations: usize,
}
struct Prepared {
    state: FreshState,
    /// The device digest that keys the fingerprint cache.
    device: [u8; 32],
    bindings: Vec<Binding>,
    batches: Vec<Batch>,
    artwork_indices: Vec<usize>,
    report: Report,
}

fn report(trace: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    durable_write(trace, name, &serde_json::to_vec_pretty(value)?)
}

fn verification(proof: &VerifiedTrackArtworkBytes) -> &'static str {
    if proof.derivative.is_none() {
        "exact_track_original_without_rendition"
    } else if proof.derivative_entropy_decode_verified {
        "exact_track_original_and_decoded_derivative"
    } else {
        "exact_track_original_and_empty_phone_rendition"
    }
}

fn prepare(journal: &Journal, current: &Current, selection: Selection) -> Result<Prepared> {
    let identity = Identity::new(&current.device, &current.library)?;
    ensure!(
        identity.device_digest() == current.identity.device_digest()
            && identity.library_digest() == current.identity.library_digest(),
        "current state identity fields disagree"
    );
    journal.ensure_identity(&identity)?;
    let mut state = FreshState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        current.receipt_sha256,
    )?;
    ensure!(
        state.library_identity().binding_sha256() == current.library.binding_sha256(),
        "current state preferences differ from library identity"
    );
    state.bind_copied_cdb(&current.cdb)?;
    state.ensure_catalog(&current.catalog)?;
    let all = journal.active_verified_bindings()?;
    let count = all.len();
    let bindings = match selection {
        Selection::All => all,
        Selection::TrackIds(ids) => {
            ensure!(
                !ids.is_empty() && ids.len() <= MAX_BINDINGS && !ids.contains(&0),
                "selected binding set is empty, zero or exceeds limit"
            );
            ensure!(
                ids.iter()
                    .all(|id| all.iter().any(|b| b.content.track_pid == *id)),
                "selected track has no active verified journal binding"
            );
            all.into_iter()
                .filter(|b| ids.contains(&b.content.track_pid))
                .collect()
        }
    };
    ensure!(
        bindings.len() <= MAX_BINDINGS,
        "verification exceeds 4096 binding limit; select a bounded subset"
    );
    let mut tracks = Vec::with_capacity(bindings.len());
    let catalog_by_id: BTreeMap<_, _> = current
        .catalog
        .tracks()
        .iter()
        .map(|t| (t.sync_id, t))
        .collect();
    ensure!(
        catalog_by_id.len() == current.catalog.tracks().len(),
        "catalog track identity is ambiguous"
    );
    let mut path_counts = BTreeMap::<String, usize>::new();
    for path in current
        .catalog
        .tracks()
        .iter()
        .filter_map(|t| t.afc_path.as_ref())
    {
        *path_counts.entry(path.to_ascii_lowercase()).or_default() += 1;
    }
    for binding in &bindings {
        if binding.content.source.is_alac() {
            binding.content.source.require_alac()?;
        } else {
            binding.content.source.validate()?;
            music_protocol::protocol::session::validate_format_music_path(
                &binding.content.path,
                binding.content.source.format(),
            )?;
        }
        let content = &binding.content;
        let track = catalog_by_id.get(&content.track_pid).copied();
        let finding = catalog_finding(
            content,
            track,
            path_counts
                .get(&content.path.to_ascii_lowercase())
                .copied()
                .unwrap_or(0),
        );
        tracks.push(TrackReport {
            track_id: format!("{:016x}", content.track_pid),
            wire_track_id: (content.track_pid as i64).to_string(),
            path: content.path.clone(),
            audio_bytes: content.source.file_bytes,
            expected_audio_sha256: hex(&content.source.full_file_sha256),
            encoded_audio_sha256: hex(&content.source.audio.sha256),
            mpeg_frame_readback: None,
            audio_proof: None,
            audio_status: if finding.is_some() {
                AudioStatus::CatalogMismatch
            } else {
                AudioStatus::NotRead
            },
            audio_observed_sha256: None,
            audio_attributes: None,
            finding,
            batch: None,
            artwork: artwork_status(content, track, &current.catalog),
        });
    }
    let eligible: Vec<_> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.finding.is_none())
        .map(|(i, t)| (i, t.audio_bytes))
        .collect();
    let (batches, audio_bytes, total_bytes) = schedule(&eligible, state.state_read_chunks()?)?;
    let artwork_indices: Vec<_> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.finding.is_none() && t.artwork.track_registration_matches == Some(true))
        .map(|(i, _)| i)
        .collect();
    let expected_artwork_bytes: Vec<_> = artwork_indices
        .iter()
        .map(|&i| {
            bindings[i]
                .content
                .artwork
                .as_ref()
                .map(|a| a.bytes)
                .context("planned artwork missing")
        })
        .collect::<Result<_>>()?;
    let artwork_bytes = artwork_read_budget(&expected_artwork_bytes, state.state_read_chunks()?)?;
    let total_bytes = total_bytes
        .checked_add(artwork_bytes)
        .context("total verification read bound overflow")?;
    ensure!(
        total_bytes <= MAX_TOTAL,
        "audio plus artwork verification exceeds 64GiB bound; select a subset"
    );
    let report = Report {
        schema: "rustunes.journal-verification",
        status: "incomplete",
        verification_completed: false,
        all_selected_audio_verified: false,
        device_identifier_sha256: hex(&identity.device_digest()),
        library_identity_sha256: hex(&identity.library_digest()),
        input_receipt_sha256: hex(&current.receipt_sha256),
        media_anchor: current.catalog.persisted_media_anchor(),
        selected_bindings: bindings.len(),
        total_verified_journal_bindings: count,
        outstanding_attempt_present: journal.outstanding()?.is_some(),
        planned_audio_bytes: audio_bytes,
        planned_total_read_bytes_upper_bound: total_bytes,
        planned_batches: batches.len(),
        completed_batches: 0,
        planned_artwork_tracks: artwork_indices.len(),
        completed_artwork_tracks: 0,
        planned_artwork_read_bytes_upper_bound: artwork_bytes,
        tracks,
        failure: None,
        phone_writes: 0,
        journal_mutations: 0,
        metadata_projection_verified: false,
        permanent_artwork_verified: false,
        atomic_snapshot_claimed: false,
        limitations: [
            "Audio equality verifies the transferred container, not the current desired source file.",
            "SQL metadata fields, playback state and unprojected catalog fields are not compared against a source projection.",
            "Journaled track originals are matched by exact bytes and derivative entropy is decoded; album representatives and source-to-derivative pixel equality are not verified.",
            "Stable per-file reads and repeated exact state guards do not form an atomic device snapshot or resolve outstanding attempts.",
        ],
    };
    Ok(Prepared {
        state,
        device: identity.device_digest(),
        bindings,
        batches,
        artwork_indices,
        report,
    })
}

fn catalog_finding(
    content: &crate::engine::state::journal::Content,
    track: Option<&Track>,
    path_references: usize,
) -> Option<&'static str> {
    let Some(track) = track else {
        return Some("track_absent_from_catalog");
    };
    if track.afc_path.as_deref() != Some(content.path.as_str()) {
        return Some("catalog_path_differs");
    }
    if path_references != 1 {
        return Some("catalog_path_is_aliased");
    }
    let a = &track.availability;
    if a.is_placeholder || !a.is_library_added || !a.has_non_purgeable_asset || !a.is_supported_type
    {
        return Some("catalog_audio_unavailable");
    }
    if track.metadata.file_size != content.source.file_bytes {
        return Some("catalog_audio_size_differs");
    }
    if content.source.file_bytes == 0 || content.source.file_bytes > MAX_AUDIO {
        return Some("audio_exceeds_64mib_read_bound");
    }
    None
}
fn artwork_status(
    content: &crate::engine::state::journal::Content,
    track: Option<&Track>,
    catalog: &Catalog,
) -> ArtworkStatus {
    let art = catalog.artwork();
    let track_id = art.track_cache_ids().get(&content.track_pid).copied();
    let album_id = track
        .and_then(|t| art.album_cache_ids().get(&t.album_sync_id))
        .copied();
    observed_artwork_status(
        content.artwork.as_ref(),
        track_id,
        album_id,
        content
            .artwork
            .as_ref()
            .is_some_and(|a| art.cached_ids().contains(&a.cache_id)),
    )
}

fn observed_artwork_status(
    expected: Option<&crate::engine::state::journal::Artwork>,
    track_id: Option<u32>,
    album_id: Option<u32>,
    cached: bool,
) -> ArtworkStatus {
    let expected_id = expected.map(|a| a.cache_id);
    let track_matches = expected_id.map(|id| track_id == Some(id) && cached);
    let album_registration = match expected_id {
        None => "not_journaled",
        Some(id) if album_id == Some(id) => "same_expected_cache",
        Some(_) if album_id.is_none() => "missing",
        Some(_) => "independent_album_representative",
    };
    let status = match expected_id {
        None => "not_journaled",
        Some(_) if track_matches == Some(true) => "track_registration_matches",
        Some(_) if track_id.is_none() => "missing",
        Some(id) if track_id == Some(id) => "cache_record_missing",
        Some(_) => "different_track_cache_unverified",
    };
    ArtworkStatus {
        expected_cache_id: expected_id,
        expected_source_cover_sha256: expected.map(|a| hex(&a.sha256)),
        observed_track_cache_id: track_id,
        observed_album_cache_id: album_id,
        track_registration_matches: track_matches,
        album_registration,
        registration: status,
        permanent_cache_verification: "not_performed",
        permanent_cache_proof: None,
        permanent_cache_failure: None,
    }
}

fn artwork_read_budget(original_bytes: &[u64], state_chunks: u64) -> Result<u64> {
    ensure!(
        original_bytes.len() <= MAX_BINDINGS,
        "too many artwork bindings"
    );
    let guards = state_chunks
        .checked_mul(2)
        .and_then(|n| n.checked_mul(CHUNK))
        .context("artwork state-read bound overflow")?;
    original_bytes.iter().try_fold(0u64, |total, &bytes| {
        ensure!(
            bytes > 0 && bytes <= artwork::MAX_IMAGE_BYTES,
            "journal cover exceeds per-image bound"
        );
        let next = total
            .checked_add(bytes)
            .and_then(|n| n.checked_add(artwork::MAX_IMAGE_BYTES))
            .and_then(|n| n.checked_add(guards))
            .context("artwork read bound overflow")?;
        ensure!(
            next <= MAX_TOTAL,
            "artwork reads exceed 64GiB bound; select a subset"
        );
        Ok(next)
    })
}

fn schedule(files: &[(usize, u64)], state_chunks: u64) -> Result<(Vec<Batch>, u64, u64)> {
    ensure!(
        files.len() <= MAX_BINDINGS,
        "binding schedule exceeds limit"
    );
    let fixed = state_chunks
        .checked_mul(2)
        .and_then(|n| n.checked_add(128))
        .context("state operation budget overflow")?;
    ensure!(
        fixed < MAX_BOUNDED_OPS as u64,
        "state guards exceed one connection budget"
    );
    let guard_bytes = state_chunks
        .checked_mul(CHUNK)
        .and_then(|n| n.checked_mul(2))
        .context("state byte budget overflow")?;
    let mut batches = Vec::new();
    let mut indices = Vec::new();
    let mut operations = fixed;
    let mut audio_bytes = 0u64;
    for &(index, size) in files {
        ensure!(
            size > 0 && size <= MAX_AUDIO,
            "scheduled audio exceeds per-file bound"
        );
        let cost = size.div_ceil(REQUEST) + 6;
        ensure!(
            fixed + cost <= MAX_BOUNDED_OPS as u64,
            "state plus one audio exceeds connection budget"
        );
        if operations + cost > MAX_BOUNDED_OPS as u64 {
            batches.push(Batch {
                indices: std::mem::take(&mut indices),
                operations: usize::try_from(operations)?,
            });
            operations = fixed;
        }
        indices.push(index);
        operations += cost;
        audio_bytes = audio_bytes
            .checked_add(size)
            .context("audio byte budget overflow")?;
    }
    if !indices.is_empty() {
        batches.push(Batch {
            indices,
            operations: usize::try_from(operations)?,
        });
    }
    let total = guard_bytes
        .checked_mul(batches.len() as u64)
        .and_then(|n| n.checked_add(audio_bytes))
        .context("total verification byte budget overflow")?;
    ensure!(
        total <= MAX_TOTAL,
        "planned state and audio reads exceed 64GiB; select fewer bindings"
    );
    Ok((batches, audio_bytes, total))
}

trait ReadBatch {
    async fn state_guard(&mut self, state: &FreshState) -> Result<()>;
    async fn attributes(&mut self, path: &str) -> Result<Option<Attributes>>;
    /// Each path's attributes; a connection may ask for them all at once.
    async fn attributes_all(&mut self, paths: &[&str]) -> Result<Vec<Option<Attributes>>> {
        let mut all = Vec::with_capacity(paths.len());
        for path in paths {
            all.push(self.attributes(path).await?);
        }
        Ok(all)
    }
    async fn audio_hash(&mut self, path: &str, size: u64) -> Result<[u8; 32]>;
    async fn mpeg_bytes(&mut self, path: &str, size: u64) -> Result<Vec<u8>>;
}
impl ReadBatch for Transfer {
    async fn state_guard(&mut self, state: &FreshState) -> Result<()> {
        state.verify_state(self).await
    }
    async fn attributes(&mut self, path: &str) -> Result<Option<Attributes>> {
        self.stat(path).await
    }
    async fn attributes_all(&mut self, paths: &[&str]) -> Result<Vec<Option<Attributes>>> {
        self.stat_all(paths).await
    }
    async fn audio_hash(&mut self, path: &str, size: u64) -> Result<[u8; 32]> {
        self.read_hash(path, size, MAX_AUDIO).await
    }
    async fn mpeg_bytes(&mut self, path: &str, size: u64) -> Result<Vec<u8>> {
        // Same cap as the native sender and existing managed readback. Only one
        // MPEG file is retained in memory; the readonly summary logs no payload.
        ensure!(
            (1..=MAX_AUDIO).contains(&size),
            "MPEG read exceeds native transfer bound"
        );
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(usize::try_from(size)?)?;
        self.read_exact(path, size, MAX_AUDIO, &mut bytes).await?;
        Ok(bytes)
    }
}
fn stable_attrs(attrs: Option<Attributes>) -> Option<Attributes> {
    attrs.map(|mut a| {
        a.remove("st_atime");
        a
    })
}
fn bounded_attributes(attrs: &Attributes) -> Result<()> {
    ensure!(attrs.len() <= 64, "audio attributes exceed entry bound");
    let bytes = attrs.iter().try_fold(0usize, |sum, (key, value)| {
        ensure!(
            key.len() <= 128 && value.len() <= 1024,
            "audio attribute exceeds field bound"
        );
        sum.checked_add(key.len())
            .and_then(|n| n.checked_add(value.len()))
            .context("audio attributes size overflow")
    })?;
    ensure!(
        bytes <= MAX_ATTRIBUTES_BYTES,
        "audio attributes exceed report bound"
    );
    Ok(())
}
fn finish_report(report: &mut Report) {
    report.verification_completed = true;
    report.all_selected_audio_verified = !report.tracks.is_empty()
        && report
            .tracks
            .iter()
            .all(|t| t.audio_status == AudioStatus::VerifiedTransferredContainer);
    report.permanent_artwork_verified = report
        .tracks
        .iter()
        .any(|t| t.artwork.expected_cache_id.is_some())
        && report
            .tracks
            .iter()
            .filter(|t| t.artwork.expected_cache_id.is_some())
            .all(|t| t.artwork.permanent_cache_proof.is_some());
    report.status = if report.tracks.is_empty() {
        "no_bindings"
    } else if report.all_selected_audio_verified
        && report.tracks.iter().all(|t| {
            t.artwork.expected_cache_id.is_none() || t.artwork.permanent_cache_proof.is_some()
        })
    {
        "audio_checks_complete"
    } else {
        "complete_with_findings"
    };
}
