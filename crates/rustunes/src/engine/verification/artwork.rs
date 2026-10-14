//! Read-only planning for the observed source300 artwork cache layout.
//! Prefer the 556 rendition, then other readback/entropy-qualified sizes.
//! A plan or SQL row is not proof of file bytes, pixels, or source equivalence.
use crate::engine::execution::device::DeviceProvider;
use crate::{
    engine::execution::native_insert::{Control, FreshState, create_trace},
    engine::state::current::Current,
};
use anyhow::{Context, Result, bail, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::artwork::Cover;
pub use music_protocol::media::renditions;
use music_protocol::protocol::trace::private_write;
use music_protocol::protocol::transfer::{
    Attributes, MAX_BOUNDED_OPS, Transfer, hex, regular_size,
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Read, Seek},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

pub const ARTWORK_ROOT: &str = "iTunes_Control/iTunes/Artwork";
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Expected permanent original bytes from a validated source cover or a
/// completed journal binding. Album artwork is a separate representative.
pub struct TrackArtworkExpectation {
    pub cache_id: u32,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// Codec-neutral identity of an expected permanent JPEG original. This is
/// read-only comparison data, not a checksum, cache registration or permission
/// to send artwork. Callers retain source/ownership/decoder proof separately.
#[derive(Clone, Copy)]
pub struct OriginalArtworkExpectation {
    pub bytes: u64,
    pub sha256: [u8; 32],
}
impl OriginalArtworkExpectation {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.bytes > 0 && self.bytes <= MAX_IMAGE_BYTES && self.sha256 != [0; 32],
            "expected artwork bytes or digest outside bound"
        );
        Ok(())
    }
}
#[cfg(test)]
impl TrackArtworkExpectation {
    fn original(&self) -> OriginalArtworkExpectation {
        OriginalArtworkExpectation {
            bytes: self.bytes,
            sha256: self.sha256,
        }
    }
}

struct TrackReadbackPlan {
    relative_path: String,
    observed_album_cache_id: Option<u32>,
}

fn track_plan(
    catalog: &Catalog,
    target_sync_id: u64,
    expected: &TrackArtworkExpectation,
) -> Result<TrackReadbackPlan> {
    ensure!(expected.cache_id >= 100, "reserved artwork cache ID");
    ensure!(
        expected.bytes > 0 && expected.bytes <= MAX_IMAGE_BYTES && expected.sha256 != [0; 32],
        "expected track artwork bytes or digest outside bound"
    );
    let mut matches = catalog
        .tracks()
        .iter()
        .filter(|t| t.sync_id == target_sync_id);
    let target = matches.next().context("artwork target absent")?;
    ensure!(matches.next().is_none(), "ambiguous artwork target");
    ensure!(
        !target.availability.is_placeholder
            && target.availability.is_library_added
            && target.availability.has_non_purgeable_asset
            && target.availability.is_supported_type
            && target.afc_path.is_some(),
        "artwork target is not supported available local audio"
    );
    let artwork = catalog.artwork();
    ensure!(
        artwork.track_cache_ids().get(&target_sync_id) == Some(&expected.cache_id),
        "target track must register the expected source300 cache"
    );
    let cached = artwork
        .cached_objects()
        .get(&(expected.cache_id, 0))
        .context("registered artwork has no supported cached object")?;
    // An album's cover is typed 6, a cover no album uses 1 (a batch member
    // whose album took another member's); both keep the same original and
    // renditions.
    ensure!(
        matches!(cached.artwork_type, 1 | 6) && cached.variant == 0,
        "unsupported cached artwork shape"
    );
    Ok(TrackReadbackPlan {
        relative_path: cached.relative_path.clone(),
        observed_album_cache_id: artwork
            .album_cache_ids()
            .get(&target.album_sync_id)
            .copied(),
    })
}

fn validate_cover(cover: &Cover) -> Result<()> {
    ensure!(
        !cover.bytes.is_empty() && cover.bytes.len() as u64 <= MAX_IMAGE_BYTES,
        "source cover outside bound"
    );
    ensure!(
        <[u8; 32]>::from(Sha256::digest(&cover.bytes)) == cover.sha256
            && u32::try_from(cover.bytes.len())?.checked_add(17) == Some(cover.checksum),
        "source cover digest or checksum mismatch"
    );
    Ok(())
}

struct AlbumReadbackPlan {
    cache_id: u32,
    relative_path: String,
}

fn album_plan(
    catalog: &Catalog,
    album_sync_id: u64,
    expected: &OriginalArtworkExpectation,
) -> Result<AlbumReadbackPlan> {
    expected.validate()?;
    album_registration(catalog, album_sync_id)?
        .map_err(|conflict| anyhow::anyhow!("album artwork conflict: {conflict}"))
}

/// The album's registered original, or the conflict that leaves it without
/// one this tool reads.
fn album_registration(
    catalog: &Catalog,
    album_sync_id: u64,
) -> Result<std::result::Result<AlbumReadbackPlan, &'static str>> {
    ensure!(album_sync_id != 0, "zero artwork album identity");
    let mut albums = catalog
        .albums()
        .values()
        .filter(|a| a.sync_id == album_sync_id);
    albums.next().context("artwork album absent")?;
    ensure!(albums.next().is_none(), "ambiguous artwork album");
    let artwork = catalog.artwork();
    let Some(&album_cache_id) = artwork.album_cache_ids().get(&album_sync_id) else {
        return Ok(Err("supported source300 registration absent"));
    };
    ensure!(album_cache_id >= 100, "reserved album artwork cache ID");
    let Some(album_cached) = artwork.cached_objects().get(&(album_cache_id, 0)) else {
        return Ok(Err("registered artwork lacks cached object"));
    };
    if album_cached.artwork_type != 6 || album_cached.variant != 0 {
        return Ok(Err("unsupported cached object shape"));
    }
    Ok(Ok(AlbumReadbackPlan {
        cache_id: album_cache_id,
        relative_path: album_cached.relative_path.clone(),
    }))
}

#[derive(Clone, Serialize)]
pub struct PermanentFile {
    pub afc_path: String,
    pub bytes: u64,
    pub sha256: String,
    pub before: Attributes,
    pub after: Attributes,
}

/// Per-track source fidelity. The shared album registration is an observation,
/// not a requirement that every track in an album contain the same picture.
#[derive(Serialize)]
pub struct VerifiedTrackArtworkBytes {
    /// One tag for every case; the derivative fields tell them apart: the
    /// selected rendition, decoded; a rendition the phone left empty, so
    /// nothing was decoded; or a cover smaller than every rendition size,
    /// which has none.
    pub schema: &'static str,
    pub target_sync_id: String,
    pub cache_id: u32,
    pub media_anchor: u32,
    pub state_receipt_sha256: String,
    pub database_sha256: String,
    pub wal_sha256: Option<String>,
    pub source_cover_sha256: String,
    pub original: PermanentFile,
    pub original_exact_source_bytes: bool,
    pub observed_album_cache_id: Option<u32>,
    pub album_representative_source_equality_claimed: bool,
    /// None for a cover smaller than every rendition size.
    pub derivative: Option<PermanentFile>,
    pub derivative_width: u16,
    pub derivative_height: u16,
    pub derivative_entropy_decode_verified: bool,
    /// None when the phone left the rendition empty.
    pub derivative_rgb24_sha256: Option<String>,
    pub derivative_rgb24_bytes: u64,
    pub derivative_source_pixel_equality_claimed: bool,
    pub decoder: &'static str,
    pub phone_writes: u32,
    /// "read" when both files were read (and the rendition decoded) now;
    /// "stat_of_verified_proof" when both kept the stats an earlier verified
    /// readback recorded, so its digests and decode result are reused.
    pub readback: &'static str,
}

pub async fn verify_track<F, Fut>(
    current: &Current,
    target: u64,
    cache: u32,
    cover: &Cover,
    trace: &Path,
    control: &Control,
    connect: F,
) -> Result<VerifiedTrackArtworkBytes>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    validate_cover(cover)?;
    verify_track_expected(
        current,
        target,
        &TrackArtworkExpectation {
            cache_id: cache,
            bytes: cover.bytes.len() as u64,
            sha256: cover.sha256,
        },
        trace,
        control,
        connect,
    )
    .await
}

/// Verifies the exact trusted expected original and the permanent derivative's
/// entropy decode. It neither reads nor changes the shared album image.
pub async fn verify_track_expected<F, Fut>(
    current: &Current,
    target: u64,
    expected: &TrackArtworkExpectation,
    trace: &Path,
    control: &Control,
    connect: F,
) -> Result<VerifiedTrackArtworkBytes>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    let selected = track_plan(&current.catalog, target, expected)?;
    let state = current_state(current, control, &BTreeSet::new())?;
    let mut afc = connect_guarded(current, &state, trace, control, 2, connect).await?;
    let proof = read_track(
        &mut afc, current, target, expected, &selected, trace, control,
    )
    .await?;
    control.check()?;
    state.verify_state(&mut afc).await?;
    drop(afc);
    write_proof(trace, &proof)?;
    control.check()?;
    Ok(proof)
}

/// One track's expected cover in a shared-connection group, and the new
/// private directory that receives its images and proof.
pub(crate) struct TrackCheck {
    pub target: u64,
    pub expected: TrackArtworkExpectation,
    pub trace: PathBuf,
}

/// Bytes per AFC read request.
const REQUEST: u64 = music_protocol::protocol::afc::copy::CHUNK as u64;
/// Operations per track beyond its exact original's reads: one derivative of
/// at most MAX_IMAGE_BYTES, plus stats, opens, closes and rendition probes.
const TRACK_EXTRA_OPS: u64 = MAX_IMAGE_BYTES.div_ceil(REQUEST) + 32;
/// Operations per guarded connection beyond both state guards' reads.
const GUARD_EXTRA_OPS: u64 = 128;
/// Covers per guarded connection at most, so that a group stays well inside
/// the time its callers allow it even on a slow Wi-Fi link.
const MAX_GROUP: usize = 64;

fn track_ops(original_bytes: u64) -> u64 {
    original_bytes
        .div_ceil(REQUEST)
        .saturating_add(TRACK_EXTRA_OPS)
}

/// How many leading originals one guarded connection's operation bound admits.
fn group_len(state_chunks: u64, originals: impl IntoIterator<Item = u64>) -> usize {
    let mut operations = state_chunks
        .saturating_mul(2)
        .saturating_add(GUARD_EXTRA_OPS);
    originals
        .into_iter()
        .take(MAX_GROUP)
        .take_while(|&bytes| {
            operations = operations.saturating_add(track_ops(bytes));
            operations <= MAX_BOUNDED_OPS as u64
        })
        .count()
}

/// The proof `verify_track_expected` gives, for each of the leading checks
/// that fit one connection's operation bound, with one pair of state guards
/// around the whole group instead of a pair per track. Proofs are written and
/// returned only after the closing guard, at least one per call. An error
/// carries the position of the check it is attributed to.
pub(crate) async fn verify_track_group<F, Fut>(
    current: &Current,
    checks: &[TrackCheck],
    trace: &Path,
    control: &Control,
    unfilled: &BTreeSet<u64>,
    connect: F,
    mut started: impl FnMut(usize) -> Result<()>,
) -> std::result::Result<Vec<VerifiedTrackArtworkBytes>, (usize, anyhow::Error)>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    let at = |position: usize| move |error: anyhow::Error| (position, error);
    let device = current.identity.device_digest();
    let opened = async {
        control.check()?;
        ensure!(!checks.is_empty(), "no artwork checks");
        let state = current_state(current, control, unfilled)?;
        let count = group_len(
            state.state_read_chunks()?,
            checks.iter().map(|c| c.expected.bytes),
        );
        Ok((state, count))
    };
    let (state, count) = opened.await.map_err(at(0))?;
    let mut plans = Vec::with_capacity(count);
    for (position, check) in checks.iter().enumerate().take(count.max(1)) {
        plans.push(
            track_plan(&current.catalog, check.target, &check.expected).map_err(at(position))?,
        );
    }
    if count == 0 {
        return Err((0, anyhow::anyhow!("artwork readback operation bound")));
    }
    let image_ops = checks[..count]
        .iter()
        .map(|c| track_ops(c.expected.bytes))
        .sum();
    let mut afc = connect_guarded_ops(current, &state, trace, control, image_ops, connect)
        .await
        .map_err(at(0))?;
    let mut reads = Vec::with_capacity(count);
    let mut failed = None;
    for (position, (check, selected)) in checks.iter().zip(&plans).enumerate() {
        let read = async {
            control.check()?;
            started(position)?;
            create_trace(&check.trace)?;
            start_track(&mut afc, &check.expected, selected, &check.trace, control).await
        };
        match read.await {
            Ok(read) => reads.push(read),
            Err(error) => {
                failed = Some((position, error));
                break;
            }
        }
    }
    // One decoder process checks the covers read while the closing guard runs.
    let held = reads
        .iter_mut()
        .filter_map(|read| read.derivative.as_mut().and_then(|d| d.held.take()))
        .collect();
    let decoding = spawn_decodes(held, control);
    let closed = match failed {
        Some(_) => Ok(()),
        None => {
            async {
                control.check()?;
                state.verify_state(&mut afc).await
            }
            .await
        }
    };
    let mut results = decoding
        .await
        .context("artwork decoder worker panicked")
        .map_err(at(0))?
        .into_iter();
    let mut proofs = Vec::with_capacity(count);
    // An earlier cover's failure is reported first.
    for (position, read) in reads.into_iter().enumerate() {
        let check = &checks[position];
        let decoded = read
            .derivative
            .as_ref()
            .is_some_and(|d| d.decodes)
            .then(|| results.next())
            .flatten();
        let proof = finish_track(
            read,
            decoded,
            current,
            check.target,
            &check.expected,
            &plans[position],
        );
        proofs.push(proof.map_err(at(position))?);
    }
    if let Some((position, error)) = failed {
        return Err((position, error));
    }
    let last = count - 1;
    closed.map_err(at(last))?;
    drop(afc);
    for (position, (check, proof)) in checks.iter().zip(&proofs).enumerate() {
        write_proof(&check.trace, proof).map_err(at(position))?;
        if let Some(cover) = cover_proof(proof) {
            crate::engine::state::audio_index::cache::cover_verified(&device, &cover);
        }
    }
    control.check().map_err(at(last))?;
    Ok(proofs)
}

/// Operations a reuse attempt may take: a stat of the original, a probe of
/// each qualified rendition size, and a stat of both files after.
const REUSE_OPS: u64 = 1 + renditions::QUALIFIED_EDGES.len() as u64 + 2;
/// Covers per reuse connection at most; each takes a few stats.
const MAX_REUSE_GROUP: usize = 512;

/// For each leading check, the cached proof of a cover whose original and
/// selected rendition keep the stats a verified readback recorded, or None
/// when it must be read. Many covers share one connection and one pair of
/// state guards; nothing is read or decoded. Proofs are written only after the
/// closing guard. An error carries the position of the check it is
/// attributed to.
pub(crate) async fn reuse_track_group<F, Fut>(
    current: &Current,
    checks: &[TrackCheck],
    trace: &Path,
    control: &Control,
    connect: F,
) -> std::result::Result<Vec<Option<VerifiedTrackArtworkBytes>>, (usize, anyhow::Error)>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    let at = |position: usize| move |error: anyhow::Error| (position, error);
    let device = current.identity.device_digest();
    let opened = async {
        control.check()?;
        ensure!(!checks.is_empty(), "no artwork checks");
        let state = current_state(current, control, &BTreeSet::new())?;
        let room = (MAX_BOUNDED_OPS as u64)
            .saturating_sub(state.state_read_chunks()?.saturating_mul(2))
            .saturating_sub(GUARD_EXTRA_OPS);
        let count = checks
            .len()
            .min(MAX_REUSE_GROUP)
            .min(usize::try_from(room / REUSE_OPS)?);
        ensure!(count > 0, "artwork readback operation bound");
        Ok((state, count))
    };
    let (state, count) = opened.await.map_err(at(0))?;
    let mut plans = Vec::with_capacity(count);
    for (position, check) in checks.iter().enumerate().take(count) {
        plans.push(
            track_plan(&current.catalog, check.target, &check.expected).map_err(at(position))?,
        );
    }
    // Only a cover with a kept proof is worth a stat.
    let known: Vec<bool> = plans
        .iter()
        .map(|selected| {
            crate::engine::state::audio_index::cache::cover_proof_known(
                &device,
                &format!("{ARTWORK_ROOT}/Originals/{}", selected.relative_path),
            )
        })
        .collect();
    if !known.contains(&true) {
        return Ok((0..count).map(|_| None).collect());
    }
    let mut afc = connect_guarded_ops(
        current,
        &state,
        trace,
        control,
        REUSE_OPS * count as u64,
        connect,
    )
    .await
    .map_err(at(0))?;
    let reads = reuse_tracks(&mut afc, &device, &checks[..count], &plans, &known, control).await?;
    let last = count - 1;
    async {
        control.check()?;
        state.verify_state(&mut afc).await
    }
    .await
    .map_err(at(last))?;
    drop(afc);
    let mut proofs = Vec::with_capacity(count);
    for (position, read) in reads.into_iter().enumerate() {
        let Some(mut read) = read else {
            proofs.push(None);
            continue;
        };
        let check = &checks[position];
        let decoded = read.reused.as_mut().and_then(Option::take).map(Ok);
        let proof = finish_track(
            read,
            decoded,
            current,
            check.target,
            &check.expected,
            &plans[position],
        )
        .map_err(at(position))?;
        write_proof(&check.trace, &proof).map_err(at(position))?;
        proofs.push(Some(proof));
    }
    control.check().map_err(at(last))?;
    Ok(proofs)
}

/// What the cover proof cache keeps of a proof read in full.
fn cover_proof(
    proof: &VerifiedTrackArtworkBytes,
) -> Option<crate::engine::state::audio_index::cache::CoverProof> {
    use crate::engine::state::audio_index::cache::{CoverProof, PhoneStat};
    // A cover without a rendition is small; it is read again each time.
    let derivative = proof.derivative.as_ref()?;
    let rgb24 = match &proof.derivative_rgb24_sha256 {
        Some(sha) => Some((proof.derivative_rgb24_bytes, hex_digest(sha).ok()?)),
        None => None,
    };
    Some(CoverProof {
        original_path: proof.original.afc_path.clone(),
        original: PhoneStat::new(&proof.original.before)?,
        original_sha256: hex_digest(&proof.original.sha256).ok()?,
        derivative_path: derivative.afc_path.clone(),
        derivative: PhoneStat::new(&derivative.before)?,
        derivative_sha256: hex_digest(&derivative.sha256).ok()?,
        width: proof.derivative_width,
        height: proof.derivative_height,
        rgb24,
    })
}

/// For each check that has a kept proof (`known`), that proof again when the
/// cover's original and selected rendition keep the stats a verified readback
/// recorded, from a stat of each file before and after and the rendition
/// probes, without reading or decoding either; None when anything differs, and
/// the caller reads the cover in full. Each step's stats are asked for all
/// covers at once. An error carries the position of its check.
async fn reuse_tracks(
    afc: &mut Transfer,
    device: &[u8; 32],
    checks: &[TrackCheck],
    plans: &[TrackReadbackPlan],
    known: &[bool],
    control: &Control,
) -> std::result::Result<Vec<Option<TrackRead>>, (usize, anyhow::Error)> {
    use crate::engine::state::audio_index::cache::{PhoneStat, verified_cover};
    let at = |position: usize| move |error: anyhow::Error| (position, error);
    let originals: Vec<String> = plans
        .iter()
        .map(|selected| format!("{ARTWORK_ROOT}/Originals/{}", selected.relative_path))
        .collect();
    // The originals, then each candidate's probes up to its proof's rendition.
    let asked: Vec<usize> = (0..checks.len()).filter(|&p| known[p]).collect();
    control.check().map_err(at(0))?;
    let before = afc
        .stat_all(
            &asked
                .iter()
                .map(|&p| originals[p].as_str())
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(at(asked.first().copied().unwrap_or(0)))?;
    let mut candidates = Vec::new();
    for (&position, original_before) in asked.iter().zip(before) {
        let check = &checks[position];
        let Some(original_before) = original_before else {
            continue;
        };
        let Some(proof) = PhoneStat::new(&original_before)
            .and_then(|stat| verified_cover(device, &originals[position], &stat))
            .filter(|proof| {
                proof.original_sha256 == check.expected.sha256
                    && proof.original.file_bytes == check.expected.bytes
            })
        else {
            continue;
        };
        let mut probes = Vec::new();
        for edge in renditions::QUALIFIED_EDGES {
            let path = renditions::derivative_path(&plans[position].relative_path, edge)
                .map_err(at(position))?;
            let last = path == proof.derivative_path;
            probes.push((edge, path));
            if last {
                candidates.push((position, original_before, proof, probes));
                break;
            }
        }
    }
    control.check().map_err(at(0))?;
    let probe_paths: Vec<&str> = candidates
        .iter()
        .flat_map(|(_, _, _, probes)| probes.iter().map(|(_, path)| path.as_str()))
        .collect();
    let mut probed = afc
        .stat_all(&probe_paths)
        .await
        .map_err(at(candidates.first().map_or(0, |c| c.0)))?
        .into_iter();
    drop(probe_paths);
    // A rendition probed before the proof's must still be absent, and the
    // proof's present with its stat.
    let mut confirmed = Vec::new();
    for (position, original_before, proof, probes) in candidates {
        let observed: Vec<_> = probed.by_ref().take(probes.len()).collect();
        let (selected_before, earlier) = observed.split_last().expect("one probe at least");
        if earlier.iter().all(Option::is_none)
            && let Some(derivative_before) = selected_before
            && PhoneStat::new(derivative_before) == Some(proof.derivative)
        {
            let derivative_before = derivative_before.clone();
            confirmed.push((position, original_before, derivative_before, proof, probes));
        }
    }
    control.check().map_err(at(0))?;
    let after_paths: Vec<&str> = confirmed
        .iter()
        .flat_map(|(position, _, _, proof, _)| {
            [
                originals[*position].as_str(),
                proof.derivative_path.as_str(),
            ]
        })
        .collect();
    let mut after = afc
        .stat_all(&after_paths)
        .await
        .map_err(at(confirmed.first().map_or(0, |c| c.0)))?
        .into_iter();
    drop(after_paths);
    let mut reads: Vec<Option<TrackRead>> = (0..checks.len()).map(|_| None).collect();
    for (position, original_before, derivative_before, proof, probes) in confirmed {
        let check = &checks[position];
        let read = (|| -> Result<TrackRead> {
            let original_after = after
                .next()
                .flatten()
                .context("permanent artwork disappeared")?;
            let derivative_after = after
                .next()
                .flatten()
                .context("permanent artwork disappeared")?;
            ensure!(
                stable_attributes(&original_before) == stable_attributes(&original_after)
                    && stable_attributes(&derivative_before)
                        == stable_attributes(&derivative_after),
                "permanent artwork changed during readback"
            );
            regular_size(&original_before, proof.original.file_bytes)?;
            regular_size(&derivative_before, proof.derivative.file_bytes)?;
            create_trace(&check.trace)?;
            let selected_edge = probes.last().expect("one probe at least").0;
            for (index, (edge, path)) in probes.iter().enumerate() {
                write_probe_receipt(&check.trace, path, *edge, index + 1 == probes.len())?;
            }
            let original = PermanentFile {
                afc_path: originals[position].clone(),
                bytes: proof.original.file_bytes,
                sha256: hex(&proof.original_sha256),
                before: original_before,
                after: original_after,
            };
            validate_expected_original(&original, &check.expected)?;
            let reused = proof.rgb24.map(|(_, rgb24_sha256)| DecodedRenditionRaster {
                width: proof.width,
                height: proof.height,
                rgb24_sha256,
            });
            Ok(TrackRead {
                original,
                derivative: Some(Derivative {
                    file: PermanentFile {
                        afc_path: proof.derivative_path.clone(),
                        bytes: proof.derivative.file_bytes,
                        sha256: hex(&proof.derivative_sha256),
                        before: derivative_before,
                        after: derivative_after,
                    },
                    edge: selected_edge,
                    decodes: reused.is_some(),
                    held: None,
                }),
                reused: Some(reused),
            })
        })();
        reads[position] = Some(read.map_err(at(position))?);
    }
    Ok(reads)
}

/// Exact original and decoded (or phone-emptied) derivative over an already
/// guarded connection.
async fn read_track(
    afc: &mut Transfer,
    current: &Current,
    target: u64,
    expected: &TrackArtworkExpectation,
    selected: &TrackReadbackPlan,
    trace: &Path,
    control: &Control,
) -> Result<VerifiedTrackArtworkBytes> {
    let mut read = start_track(afc, expected, selected, trace, control).await?;
    let decoded = match &mut read.derivative {
        Some(derivative) => decode(derivative, control).await?,
        None => None,
    };
    finish_track(read, decoded, current, target, expected, selected)
}

/// A track's exact original, and its derivative whose decode check is pending.
struct TrackRead {
    original: PermanentFile,
    /// None for a cover smaller than every rendition size.
    derivative: Option<Derivative>,
    /// For a reused proof, the decode result it recorded (None for an empty
    /// rendition); nothing is decoded again.
    reused: Option<Option<DecodedRenditionRaster>>,
}

/// Read a track's original and derivative, holding the derivative to decode.
async fn start_track(
    afc: &mut Transfer,
    expected: &TrackArtworkExpectation,
    selected: &TrackReadbackPlan,
    trace: &Path,
    control: &Control,
) -> Result<TrackRead> {
    let original_path = format!("{ARTWORK_ROOT}/Originals/{}", selected.relative_path);
    let (original, mut held) = read_file(
        afc,
        &original_path,
        &trace.join("original.jpg"),
        Some(expected.bytes),
    )
    .await?;
    validate_expected_original(&original, expected)?;
    let derivative = start_derivative(afc, &selected.relative_path, trace, control).await?;
    if derivative.is_none() {
        without_rendition(&original, &mut held)?;
    }
    Ok(TrackRead {
        original,
        derivative,
        reused: None,
    })
}

/// The track's proof with its derivative's decode check, if it has one.
fn finish_track(
    read: TrackRead,
    decoded: Option<Result<DecodedRenditionRaster>>,
    current: &Current,
    target: u64,
    expected: &TrackArtworkExpectation,
    selected: &TrackReadbackPlan,
) -> Result<VerifiedTrackArtworkBytes> {
    let TrackRead {
        original,
        derivative,
        reused,
    } = read;
    let (derivative, width, height, decoded) = match derivative {
        Some(derivative) => {
            let (file, width, height, decoded) = derivative.finish(decoded)?;
            if decoded.is_none() {
                empty_rendition(target, width)?;
            }
            (Some(file), width, height, decoded)
        }
        None => (None, 0, 0, None),
    };
    Ok(VerifiedTrackArtworkBytes {
        schema: "rustunes.permanent-track-artwork-bytes",
        target_sync_id: format!("{target:016X}"),
        cache_id: expected.cache_id,
        media_anchor: current.catalog.persisted_media_anchor(),
        state_receipt_sha256: hex(&current.receipt_sha256),
        database_sha256: hex(&current.catalog.source_hashes().database),
        wal_sha256: current.catalog.source_hashes().wal.as_ref().map(|h| hex(h)),
        source_cover_sha256: hex(&expected.sha256),
        original,
        original_exact_source_bytes: true,
        observed_album_cache_id: selected.observed_album_cache_id,
        album_representative_source_equality_claimed: false,
        derivative,
        derivative_width: width,
        derivative_height: height,
        derivative_entropy_decode_verified: decoded.is_some(),
        derivative_rgb24_sha256: decoded.map(|d| hex(&d)),
        derivative_rgb24_bytes: rgb24_bytes(decoded, width, height)?,
        derivative_source_pixel_equality_claimed: false,
        decoder: "FFmpeg MJPEG to RGB24; no resize/filter",
        phone_writes: 0,
        readback: if reused.is_some() {
            "stat_of_verified_proof"
        } else {
            "read"
        },
    })
}

fn write_proof(trace: &Path, proof: &VerifiedTrackArtworkBytes) -> Result<()> {
    private_write(
        &trace.join("verified-track-artwork-bytes.json"),
        &serde_json::to_vec_pretty(proof)?,
    )?;
    File::open(trace)?.sync_all()?;
    Ok(())
}

fn validate_expected_original(
    file: &PermanentFile,
    expected: &TrackArtworkExpectation,
) -> Result<()> {
    ensure!(
        file.bytes == expected.bytes && file.sha256 == hex(&expected.sha256),
        "permanent original differs from expected track cover bytes"
    );
    Ok(())
}

/// Exact existing album-original equivalence, independent of track cache IDs.
/// This makes no derivative, UI-pixel, audio, or mutation-completion claim.
#[derive(Serialize)]
pub struct VerifiedAlbumOriginal {
    pub schema: &'static str,
    pub album_sync_id: String,
    pub cache_id: u32,
    pub media_anchor: u32,
    pub state_receipt_sha256: String,
    pub database_sha256: String,
    pub wal_sha256: Option<String>,
    pub source_cover_sha256: String,
    pub original: PermanentFile,
    pub original_exact_source_bytes: bool,
    pub derivative_verified: bool,
    pub phone_writes: u32,
}

pub async fn verify_album_original<F, Fut>(
    current: &Current,
    album_sync_id: u64,
    cover: &Cover,
    trace: &Path,
    control: &Control,
    connect: F,
) -> Result<VerifiedAlbumOriginal>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    validate_cover(cover)?;
    verify_album_original_expected(
        current,
        album_sync_id,
        &OriginalArtworkExpectation {
            bytes: cover.bytes.len() as u64,
            sha256: cover.sha256,
        },
        trace,
        control,
        connect,
    )
    .await
}

/// Compare an album's registered permanent original with exact image bytes.
/// This does not infer an MP4/ID3 checksum or authorize an artwork operation.
pub async fn verify_album_original_expected<F, Fut>(
    current: &Current,
    album_sync_id: u64,
    expected: &OriginalArtworkExpectation,
    trace: &Path,
    control: &Control,
    connect: F,
) -> Result<VerifiedAlbumOriginal>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    let selected = album_plan(&current.catalog, album_sync_id, expected)?;
    let state = current_state(current, control, &BTreeSet::new())?;
    let mut afc = connect_guarded(current, &state, trace, control, 1, connect).await?;
    let path = format!("{ARTWORK_ROOT}/Originals/{}", selected.relative_path);
    let (original, _) = read_file(
        &mut afc,
        &path,
        &trace.join("album-original.jpg"),
        Some(expected.bytes),
    )
    .await?;
    validate_original(&original, expected)
        .context("album artwork conflict: permanent original differs from desired cover")?;
    control.check()?;
    state.verify_state(&mut afc).await?;
    drop(afc);
    let proof = VerifiedAlbumOriginal {
        schema: "rustunes.permanent-album-original",
        album_sync_id: format!("{album_sync_id:016X}"),
        cache_id: selected.cache_id,
        media_anchor: current.catalog.persisted_media_anchor(),
        state_receipt_sha256: hex(&current.receipt_sha256),
        database_sha256: hex(&current.catalog.source_hashes().database),
        wal_sha256: current.catalog.source_hashes().wal.as_ref().map(|h| hex(h)),
        source_cover_sha256: hex(&expected.sha256),
        original,
        original_exact_source_bytes: true,
        derivative_verified: false,
        phone_writes: 0,
    };
    private_write(
        &trace.join("verified-album-original.json"),
        &serde_json::to_vec_pretty(&proof)?,
    )?;
    File::open(trace)?.sync_all()?;
    control.check()?;
    Ok(proof)
}

/// Whether an album's registered permanent original is exactly `expected`.
/// `Ok(Err(reason))` when it is not: the album registers no image this tool
/// reads, or its original holds other bytes. An error is a failure to find
/// out (bounds, state or transport), which shows neither.
pub async fn album_original_equal<F, Fut>(
    current: &Current,
    album_sync_id: u64,
    expected: &OriginalArtworkExpectation,
    trace: &Path,
    control: &Control,
    connect: F,
) -> Result<std::result::Result<VerifiedAlbumOriginal, String>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    expected.validate()?;
    let selected = match album_registration(&current.catalog, album_sync_id)? {
        Ok(selected) => selected,
        Err(conflict) => return Ok(Err(conflict.into())),
    };
    let state = current_state(current, control, &BTreeSet::new())?;
    let mut afc = connect_guarded(current, &state, trace, control, 1, connect).await?;
    let path = format!("{ARTWORK_ROOT}/Originals/{}", selected.relative_path);
    let (original, _) = read_file(&mut afc, &path, &trace.join("album-original.jpg"), None).await?;
    control.check()?;
    state.verify_state(&mut afc).await?;
    drop(afc);
    if original.bytes != expected.bytes || original.sha256 != hex(&expected.sha256) {
        return Ok(Err(format!(
            "permanent original ({} bytes, SHA-256 {}) is other bytes",
            original.bytes, original.sha256
        )));
    }
    let proof = VerifiedAlbumOriginal {
        schema: "rustunes.permanent-album-original",
        album_sync_id: format!("{album_sync_id:016X}"),
        cache_id: selected.cache_id,
        media_anchor: current.catalog.persisted_media_anchor(),
        state_receipt_sha256: hex(&current.receipt_sha256),
        database_sha256: hex(&current.catalog.source_hashes().database),
        wal_sha256: current.catalog.source_hashes().wal.as_ref().map(|h| hex(h)),
        source_cover_sha256: hex(&expected.sha256),
        original,
        original_exact_source_bytes: true,
        derivative_verified: false,
        phone_writes: 0,
    };
    private_write(
        &trace.join("verified-album-original.json"),
        &serde_json::to_vec_pretty(&proof)?,
    )?;
    File::open(trace)?.sync_all()?;
    control.check()?;
    Ok(Ok(proof))
}

// This narrow seam lets offline fixtures exercise selection and real decoding
// without a device connector. Present files never turn back into candidates.
trait DerivativeReader {
    async fn stat(&mut self, path: &str) -> Result<Option<Attributes>>;
    async fn read_observed(
        &mut self,
        path: &str,
        local: &Path,
        before: Attributes,
    ) -> Result<(PermanentFile, File)>;
}
impl DerivativeReader for Transfer {
    async fn stat(&mut self, path: &str) -> Result<Option<Attributes>> {
        Transfer::stat(self, path).await
    }
    async fn read_observed(
        &mut self,
        path: &str,
        local: &Path,
        before: Attributes,
    ) -> Result<(PermanentFile, File)> {
        read_observed_file(self, path, local, None, Some(before)).await
    }
}

/// The decoded raster's size; zero when the phone left the rendition empty.
fn rgb24_bytes(decoded: Option<[u8; 32]>, width: u16, height: u16) -> Result<u64> {
    match decoded {
        Some(_) => renditions::rgb24_bytes(width, height),
        None => Ok(0),
    }
}

/// Read and decode one rendition as `verify_track_expected` does.
#[cfg(test)]
async fn read_derivative(
    afc: &mut impl DerivativeReader,
    relative: &str,
    trace: &Path,
    control: &Control,
) -> Result<Option<(PermanentFile, u16, u16, Option<[u8; 32]>)>> {
    let Some(mut derivative) = start_derivative(afc, relative, trace, control).await? else {
        return Ok(None);
    };
    let decoded = decode(&mut derivative, control).await?;
    derivative.finish(decoded).map(Some)
}

/// Report a cover whose original is exact but whose rendition the phone left
/// empty (seen for a progressive grayscale JPEG original).
fn empty_rendition(target: u64, edge: u16) -> Result<()> {
    crate::engine::observe(crate::engine::Event::Observation(
        crate::engine::Observation::EmptyRendition {
            track: target,
            edge,
        },
    ))
}

/// The phone makes no rendition larger than the original: a 500x500 cover's
/// largest is 480, and a 64x48 one got none at any size. A cover whose longer
/// side is under every qualified rendition size therefore has none, and its
/// original, read back exactly (`held`, whose digest `original` records), is
/// the whole proof. Any larger cover without a qualified rendition refuses.
fn without_rendition(original: &PermanentFile, held: &mut File) -> Result<()> {
    let mut bytes = Vec::new();
    held.seek(io::SeekFrom::Start(0))?;
    held.read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == original.bytes && hex(&Sha256::digest(&bytes)) == original.sha256,
        "held original changed"
    );
    let (width, height) = music_protocol::media::jpeg::source_dimensions(&bytes)?;
    let smallest = renditions::QUALIFIED_EDGES.into_iter().min().unwrap_or(0);
    ensure!(
        width.max(height) < smallest,
        "no qualified permanent artwork rendition available"
    );
    Ok(())
}

/// A rendition read from the phone, held for its decode check unless the
/// phone left it empty.
struct Derivative {
    file: PermanentFile,
    edge: u16,
    /// Whether the rendition has a decode check; `held` is taken to run it.
    decodes: bool,
    held: Option<HeldRendition>,
}
impl Derivative {
    fn finish(
        self,
        decoded: Option<Result<DecodedRenditionRaster>>,
    ) -> Result<(PermanentFile, u16, u16, Option<[u8; 32]>)> {
        let Some(decoded) = decoded else {
            ensure!(!self.decodes, "artwork decode check missing");
            return Ok((self.file, self.edge, self.edge, None));
        };
        let decoded = decoded?;
        Ok((
            self.file,
            decoded.width,
            decoded.height,
            Some(decoded.rgb24_sha256),
        ))
    }
}

/// The decode check of one derivative, on a blocking worker.
async fn decode(
    derivative: &mut Derivative,
    control: &Control,
) -> Result<Option<Result<DecodedRenditionRaster>>> {
    let Some(held) = derivative.held.take() else {
        return Ok(None);
    };
    let mut decoded = spawn_decodes(vec![held], control)
        .await
        .context("artwork decoder worker panicked")?;
    Ok(decoded.pop())
}

/// Run the decode checks of these renditions on a blocking worker.
fn spawn_decodes(
    held: Vec<HeldRendition>,
    control: &Control,
) -> tokio::task::JoinHandle<Vec<Result<DecodedRenditionRaster>>> {
    let control = Control {
        deadline: control.deadline,
        cancelled: Arc::clone(&control.cancelled),
    };
    tokio::task::spawn_blocking(move || verify_held_renditions(held, &control))
}

/// Read the selected rendition and hold it for its decode check.
async fn start_derivative(
    afc: &mut impl DerivativeReader,
    relative: &str,
    trace: &Path,
    control: &Control,
) -> Result<Option<Derivative>> {
    renditions::validate_relative_path(relative)?;
    let mut selected = None;
    for edge in renditions::QUALIFIED_EDGES {
        control.check()?;
        let path = renditions::derivative_path(relative, edge)?;
        let observed = afc.stat(&path).await?;
        write_probe_receipt(trace, &path, edge, observed.is_some())?;
        if let Some(before) = observed {
            selected = Some((edge, path, before));
            break;
        }
    }
    let Some((edge, path, before)) = selected else {
        return Ok(None);
    };
    let size: u64 = before
        .get("st_size")
        .context("artwork size absent")?
        .parse()?;
    if size == 0 {
        // The phone created the rendition and wrote nothing; two identical
        // stats are the whole observation, and there is nothing to decode.
        regular_size(&before, 0)?;
        control.check()?;
        let after = afc
            .stat(&path)
            .await?
            .context("permanent artwork disappeared")?;
        ensure!(
            stable_attributes(&before) == stable_attributes(&after),
            "permanent artwork changed during readback"
        );
        let file = PermanentFile {
            afc_path: path,
            bytes: 0,
            sha256: hex(&Sha256::digest([])),
            before,
            after,
        };
        return Ok(Some(Derivative {
            file,
            edge,
            decodes: false,
            held: None,
        }));
    }
    // Once Present is selected, any read/hash/raster/decode failure is terminal.
    let (derivative, held) = afc
        .read_observed(&path, &trace.join("derivative.jpg"), before)
        .await?;
    let held = HeldRendition {
        held,
        edge,
        bytes: derivative.bytes,
        sha256: hex_digest(&derivative.sha256)?,
    };
    Ok(Some(Derivative {
        file: derivative,
        edge,
        decodes: true,
        held: Some(held),
    }))
}

/// One rendition probe's receipt. Transfer's None is deliberately not promoted
/// to canonical absence; a subsequent candidate can establish positive
/// integrity independently.
fn write_probe_receipt(trace: &Path, path: &str, edge: u16, present: bool) -> Result<()> {
    let receipt = serde_json::json!({
        "schema": "rustunes.artwork-rendition-probe",
        "afc_path": path,
        "edge": edge,
        "observation": if present {"present"} else {"unavailable_unconfirmed"},
        "canonical_absence_claimed": false,
        "phone_writes": 0
    });
    let receipt_path = trace.join(format!("derivative-probe-{edge}.json"));
    private_write(&receipt_path, &serde_json::to_vec_pretty(&receipt)?)?;
    Ok(())
}

/// AFC attributes without the access time, which reading may change.
fn stable_attributes(attrs: &Attributes) -> Attributes {
    attrs
        .iter()
        .filter(|(k, _)| k.as_str() != "st_atime")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub(crate) struct DecodedRenditionRaster {
    pub width: u16,
    pub height: u16,
    pub rgb24_sha256: [u8; 32],
}

/// A rendition read from the phone into a held local file, with the size and
/// SHA-256 of the bytes read.
struct HeldRendition {
    held: File,
    edge: u16,
    bytes: u64,
    sha256: [u8; 32],
}

/// The decode check of each rendition, as `verify_held_rendition` gives it,
/// with one decoder process for them all. A batch that fails in any way
/// checks every rendition again on its own, for its exact verdict.
fn verify_held_renditions(
    renditions: Vec<HeldRendition>,
    control: &Control,
) -> Vec<Result<DecodedRenditionRaster>> {
    if renditions.len() > 1
        && let Ok(decoded) = decode_batch(&renditions, control)
    {
        return decoded.into_iter().map(Ok).collect();
    }
    renditions
        .into_iter()
        .map(|r| verify_held_rendition(r.held, r.edge, r.bytes, r.sha256, control))
        .collect()
}

/// One FFmpeg process decodes the renditions, in order, to RGB24 frame
/// hashes: the same flags as `decode_jpeg_to_bound`, plus `-noautoscale`,
/// without which FFmpeg scales every frame to the first one's size.
fn decode_batch(
    renditions: &[HeldRendition],
    control: &Control,
) -> Result<Vec<DecodedRenditionRaster>> {
    control.check()?;
    let mut checked = Vec::with_capacity(renditions.len());
    for r in renditions {
        let mut held = r.held.try_clone()?;
        let (before, width, height) = held_geometry(&mut held, r.edge, r.bytes, r.sha256)?;
        checked.push((
            before,
            width,
            height,
            renditions::rgb24_bytes(width, height)?,
        ));
    }
    let mut files = renditions
        .iter()
        .map(|r| r.held.try_clone())
        .collect::<io::Result<Vec<_>>>()?;
    let (input, mut feed) = io::pipe()?;
    let mut command = Command::new("ffmpeg");
    command
        .env_remove("FFREPORT")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-max_error_rate",
            "0",
            "-err_detect",
            "explode",
            "-protocol_whitelist",
            "file,pipe",
            "-threads",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "jpeg_pipe",
            "-i",
            "/proc/self/fd/0",
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-fps_mode",
            "passthrough",
            "-threads",
            "1",
            "-noautoscale",
            "-pix_fmt",
            "rgb24",
            "-f",
            "framehash",
            "-hash",
            "sha256",
            "pipe:1",
        ])
        .stdin(input);
    let feeding =
        thread::Builder::new()
            .name("artwork-feed".into())
            .spawn(move || -> io::Result<()> {
                for file in &mut files {
                    file.rewind()?;
                    io::copy(file, &mut feed)?;
                }
                Ok(())
            })?;
    let frames = renditions.len();
    let hashes = supervise(
        &mut command,
        move |output| frame_hashes(output, frames),
        control.deadline.into_std(),
        Arc::clone(&control.cancelled),
    );
    // Close this process's end of the pipe, so a feed the decoder stopped
    // reading from ends.
    drop(command);
    let fed = feeding
        .join()
        .map_err(|_| anyhow::anyhow!("JPEG feed panicked"))?;
    let hashes = hashes?;
    fed?;
    let mut decoded = Vec::with_capacity(frames);
    for ((r, (before, width, height, rgb24_bytes)), (size, rgb24_sha256)) in
        renditions.iter().zip(checked).zip(hashes)
    {
        ensure!(size == rgb24_bytes, "decoded JPEG frame size differs");
        ensure!(
            unchanged(&before, &r.held.metadata()?),
            "held JPEG changed across hashing/decoding"
        );
        decoded.push(DecodedRenditionRaster {
            width,
            height,
            rgb24_sha256,
        });
    }
    control.check()?;
    Ok(decoded)
}

/// Each frame's size and SHA-256 from FFmpeg's framehash output, which must
/// list exactly `frames` frames of its one stream.
fn frame_hashes(output: impl Read, frames: usize) -> Result<Vec<(u64, [u8; 32])>> {
    let cap = 4096 + 128 * frames as u64;
    let mut text = String::new();
    output.take(cap + 1).read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= cap, "framehash output exceeds bound");
    let hashes = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            ensure!(
                fields.len() == 6 && fields[0] == "0",
                "unexpected framehash line"
            );
            Ok((fields[4].parse()?, hex_digest(fields[5])?))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(hashes.len() == frames, "decoded JPEG frame count differs");
    Ok(hashes)
}

/// A held file's size, type and geometry for its rendition, after checking
/// its bytes still hash as read.
fn held_geometry(
    held: &mut File,
    edge: u16,
    expected_bytes: u64,
    expected_sha256: [u8; 32],
) -> Result<(fs::Metadata, u16, u16)> {
    ensure!(
        renditions::is_qualified_edge(edge),
        "unqualified artwork rendition geometry"
    );
    let before = held.metadata()?;
    ensure!(
        before.is_file()
            && before.len() == expected_bytes
            && (1..=MAX_IMAGE_BYTES).contains(&expected_bytes),
        "held artwork size/type differs"
    );
    held.rewind()?;
    let mut bytes = Vec::with_capacity(usize::try_from(expected_bytes)?);
    held.by_ref()
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == expected_bytes
            && <[u8; 32]>::from(Sha256::digest(&bytes)) == expected_sha256,
        "local derivative changed"
    );
    // The rendition fills its cache path's box along the longer side and keeps
    // the cover's aspect: a 720x719 cover gives 556x555 (the reference MP3's).
    let (width, height) = jpeg_dimensions(&bytes)?;
    ensure!(
        width.max(height) == edge && width.min(height) > 0,
        "derivative geometry differs from selected cache path"
    );
    held.rewind()?;
    Ok((before, width, height))
}

/// Whether a held file is unchanged between two of its stats.
fn unchanged(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

/// Synchronous offline integrity check of an already held regular file. This
/// uses the same bounded decoder as permanent artwork verification, never a
/// fresh path open, new deadline, source-pixel claim or mutation capability.
pub(crate) fn verify_held_rendition(
    mut held: File,
    edge: u16,
    expected_bytes: u64,
    expected_sha256: [u8; 32],
    control: &Control,
) -> Result<DecodedRenditionRaster> {
    control.check()?;
    let (before, width, height) = held_geometry(&mut held, edge, expected_bytes, expected_sha256)?;
    let retained = held.try_clone()?;
    let decoded = decode_jpeg(
        held,
        width,
        height,
        control.deadline.into_std(),
        Arc::clone(&control.cancelled),
    )?;
    ensure!(
        unchanged(&before, &retained.metadata()?),
        "held JPEG changed across hashing/decoding"
    );
    control.check()?;
    Ok(DecodedRenditionRaster {
        width,
        height,
        rgb24_sha256: decoded,
    })
}

fn hex_digest(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid artwork digest"
    );
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[2 * i..2 * i + 2], 16)?;
    }
    Ok(digest)
}

/// `unfilled` names the rows an unfinished insertion left without a file
/// location; only those may lack one.
fn current_state(
    current: &Current,
    control: &Control,
    unfilled: &BTreeSet<u64>,
) -> Result<FreshState> {
    control.check()?;
    ensure!(
        control.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(120),
        "artwork readback exceeds 120-second budget"
    );
    let library = music_protocol::catalogue::identity::parse(&current.raw_preferences)?;
    library.ensure_matches(&music_protocol::catalogue::identity::parse(
        &current.plist_preferences,
    )?)?;
    library.ensure_matches(&current.library)?;
    let identity = crate::engine::state::journal::Identity::new(&current.device, &library)?;
    ensure!(
        identity.device_digest() == current.identity.device_digest()
            && identity.library_digest() == current.identity.library_digest(),
        "current library identity binding disagrees"
    );
    let mut state = FreshState::for_unfilled(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        current.receipt_sha256,
        unfilled,
    )?;
    state.bind_copied_cdb(&current.cdb)?;
    Ok(state)
}

async fn connect_guarded<F, Fut>(
    current: &Current,
    state: &FreshState,
    trace: &Path,
    control: &Control,
    image_reads: u64,
    connect: F,
) -> Result<Transfer>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    ensure!(
        (1..=2).contains(&image_reads),
        "invalid artwork image-read bound"
    );
    connect_guarded_ops(
        current,
        state,
        trace,
        control,
        image_reads * MAX_IMAGE_BYTES.div_ceil(65536),
        connect,
    )
    .await
}

/// A connection whose budget is both state guards plus `image_ops` image
/// operations, checked by the opening guard.
async fn connect_guarded_ops<F, Fut>(
    current: &Current,
    state: &FreshState,
    trace: &Path,
    control: &Control,
    image_ops: u64,
    connect: F,
) -> Result<Transfer>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    let operations = usize::try_from(
        state
            .state_read_chunks()?
            .checked_mul(2)
            .and_then(|n| n.checked_add(image_ops))
            .and_then(|n| n.checked_add(GUARD_EXTRA_OPS))
            .context("artwork operation budget overflow")?,
    )?;
    ensure!(
        operations <= MAX_BOUNDED_OPS,
        "artwork readback operation bound"
    );
    create_trace(trace)?;
    let provider = timeout_at(
        control
            .deadline
            .min(Instant::now() + Duration::from_secs(15)),
        connect(),
    )
    .await
    .context("artwork USB discovery deadline")??;
    ensure!(
        provider.udid == current.device,
        "artwork readback device differs from current state"
    );
    let mut afc = Transfer::connect_bounded_readonly_summary(
        &provider,
        trace,
        control.deadline,
        operations,
        Arc::clone(&control.cancelled),
    )
    .await?;
    state.verify_state(&mut afc).await?;
    Ok(afc)
}

/// A file of `expected` bytes is read on the read's own stats; one of unknown
/// size is stat first for its size.
async fn read_file(
    afc: &mut Transfer,
    path: &str,
    local: &Path,
    expected: Option<u64>,
) -> Result<(PermanentFile, File)> {
    let before = match expected {
        Some(_) => None,
        None => Some(
            afc.stat(path)
                .await?
                .context("permanent artwork file absent")?,
        ),
    };
    read_observed_file(afc, path, local, expected, before).await
}

/// Read a file whose size is `expected` or that `before` (a stat taken
/// earlier) gives. The read stats the file before opening and after closing
/// it; `before` must agree with both.
async fn read_observed_file(
    afc: &mut Transfer,
    path: &str,
    local: &Path,
    expected: Option<u64>,
    before: Option<Attributes>,
) -> Result<(PermanentFile, File)> {
    let size = match (&before, expected) {
        (Some(before), _) => {
            let size: u64 = before
                .get("st_size")
                .context("artwork size absent")?
                .parse()?;
            regular_size(before, size)?;
            size
        }
        (None, Some(size)) => size,
        (None, None) => bail!("artwork size unknown"),
    };
    ensure!(
        (1..=MAX_IMAGE_BYTES).contains(&size) && expected.is_none_or(|n| n == size),
        "permanent artwork size {size} differs from {expected:?} or exceeds bound"
    );
    let mut held = OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .mode(0o600)
        .open(local)?;
    let read = afc
        .read_exact_observed(path, size, MAX_IMAGE_BYTES, &mut held)
        .await?;
    held.sync_all()?;
    ensure!(
        before.is_none_or(|before| stable_attributes(&before) == stable_attributes(&read.before))
            && stable_attributes(&read.before) == stable_attributes(&read.after),
        "permanent artwork changed during readback"
    );
    let (sha, before, after) = (read.sha256, read.before, read.after);
    let written = held.metadata()?;
    let path_metadata = fs::symlink_metadata(local)?;
    ensure!(
        path_metadata.is_file(),
        "local artwork path is not a regular file"
    );
    let readonly = File::open(local)?;
    for observed in [path_metadata, readonly.metadata()?] {
        ensure!(
            written.dev() == observed.dev()
                && written.ino() == observed.ino()
                && written.len() == observed.len()
                && written.mtime() == observed.mtime()
                && written.mtime_nsec() == observed.mtime_nsec()
                && written.ctime() == observed.ctime()
                && written.ctime_nsec() == observed.ctime_nsec(),
            "local artwork handle changed"
        );
    }
    Ok((
        PermanentFile {
            afc_path: path.to_owned(),
            bytes: size,
            sha256: hex(&sha),
            before,
            after,
        },
        readonly,
    ))
}
fn validate_original(
    original: &PermanentFile,
    expected: &OriginalArtworkExpectation,
) -> Result<()> {
    expected.validate()?;
    ensure!(
        original.bytes == expected.bytes && original.sha256 == hex(&expected.sha256),
        "permanent original does not equal the exact source cover"
    );
    Ok(())
}

/// Bounded marker qualification, not entropy decoding. Require one complete
/// baseline, 8-bit, three-component JPEG with no trailing second image or data.
pub(crate) use music_protocol::media::jpeg::dimensions as jpeg_dimensions;

const LOG_CAP: usize = 64 * 1024;
fn decode_jpeg(
    held: File,
    width: u16,
    height: u16,
    deadline: std::time::Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<[u8; 32]> {
    let expected_rgb_bytes = renditions::rgb24_bytes(width, height)?;
    decode_jpeg_to_bound(held, expected_rgb_bytes, deadline, cancelled)
}

// A rendition's longer side must equal its qualified cache edge
// (`held_geometry`); a source APIC has no such edge and may have any geometry.
fn decode_jpeg_to_bound(
    held: File,
    expected_rgb_bytes: u64,
    deadline: std::time::Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<[u8; 32]> {
    let metadata_before = held.metadata()?;
    let mut command = Command::new("ffmpeg");
    command
        .env_remove("FFREPORT")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-max_error_rate",
            "0",
            "-err_detect",
            "explode",
            "-protocol_whitelist",
            "file,pipe",
            "-threads",
            "1",
            "-c:v",
            "mjpeg",
            "-f",
            "jpeg_pipe",
            "-i",
            "/proc/self/fd/0",
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
            "-frames:v",
            "1",
            "-threads",
            "1",
            "-pix_fmt",
            "rgb24",
            "-c:v",
            "rawvideo",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::from(held.try_clone()?));
    let digest = supervise(
        &mut command,
        move |output| output_hash(output, expected_rgb_bytes),
        deadline,
        cancelled,
    )?;
    let after = held.metadata()?;
    ensure!(
        metadata_before.dev() == after.dev()
            && metadata_before.ino() == after.ino()
            && metadata_before.len() == after.len()
            && metadata_before.mtime() == after.mtime()
            && metadata_before.mtime_nsec() == after.mtime_nsec()
            && metadata_before.ctime() == after.ctime()
            && metadata_before.ctime_nsec() == after.ctime_nsec(),
        "held JPEG changed during decode"
    );
    Ok(digest)
}

struct Log {
    bytes: Vec<u8>,
    truncated: bool,
}
fn output_hash(mut input: impl Read, expected: u64) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut left = expected;
    let mut buffer = [0; 65536];
    while left > 0 {
        let want = left.min(buffer.len() as u64) as usize;
        let n = input.read(&mut buffer[..want])?;
        ensure!(n > 0, "decoded JPEG output truncated");
        digest.update(&buffer[..n]);
        left -= n as u64;
    }
    ensure!(
        input.read(&mut buffer[..1])? == 0,
        "decoded JPEG exceeds pixel bound"
    );
    Ok(digest.finalize().into())
}
fn drain_log(mut input: impl Read) -> Result<Log> {
    let mut log = Log {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0; 4096];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            return Ok(log);
        }
        let keep = n.min(LOG_CAP - log.bytes.len());
        log.bytes.extend_from_slice(&buffer[..keep]);
        log.truncated |= keep < n;
    }
}
fn join<T>(worker: JoinHandle<Result<T>>) -> Result<T> {
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("JPEG pipe reader panicked"))?
}
/// Run the decoder with `read` consuming its output, within the deadline less
/// two seconds; it must exit cleanly with nothing on stderr.
fn supervise<T: Send + 'static>(
    command: &mut Command,
    read: impl FnOnce(ChildStdout) -> Result<T> + Send + 'static,
    deadline: std::time::Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<T> {
    let stop = deadline
        .checked_sub(Duration::from_secs(2))
        .context("invalid JPEG decode deadline")?;
    ensure!(
        std::time::Instant::now() < stop && !cancelled.load(Ordering::Relaxed),
        "JPEG decode cancelled or budget exhausted before spawn"
    );
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting FFmpeg JPEG decoder")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut output = None;
    let mut diagnostics = None;
    let started = (|| -> Result<()> {
        output = Some(
            thread::Builder::new()
                .name("artwork-rgb".into())
                .spawn(move || read(stdout))?,
        );
        diagnostics = Some(
            thread::Builder::new()
                .name("artwork-diagnostics".into())
                .spawn(move || drain_log(stderr))?,
        );
        Ok(())
    })();
    if let Err(error) = started {
        let _ = child.kill();
        cleanup(&mut child, &mut output, &mut diagnostics, deadline)?;
        return Err(error.context("starting JPEG pipe readers"));
    }
    let result = (|| {
        let mut pixels = None;
        let mut log: Option<Log> = None;
        loop {
            if output.as_ref().is_some_and(JoinHandle::is_finished) {
                pixels = Some(join(output.take().unwrap())?);
            }
            if diagnostics.as_ref().is_some_and(JoinHandle::is_finished) {
                log = Some(join(diagnostics.take().unwrap())?);
            }
            if pixels.is_some()
                && let Some(status) = child.try_wait()?
                && let Some(log) = log.as_ref()
            {
                let pixels = pixels.take().unwrap();
                ensure!(
                    status.success() && log.bytes.is_empty() && !log.truncated,
                    "FFmpeg JPEG decode failed (status {status}): {}{}",
                    String::from_utf8_lossy(&log.bytes),
                    if log.truncated { "[truncated]" } else { "" }
                );
                return Ok(pixels);
            }
            ensure!(
                std::time::Instant::now() < stop && !cancelled.load(Ordering::Relaxed),
                "JPEG decode cancelled or deadline expired"
            );
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    cleanup(&mut child, &mut output, &mut diagnostics, deadline)?;
    result
}
fn cleanup<T>(
    child: &mut Child,
    output: &mut Option<JoinHandle<Result<T>>>,
    diagnostics: &mut Option<JoinHandle<Result<Log>>>,
    deadline: std::time::Instant,
) -> Result<()> {
    loop {
        if child.try_wait()?.is_some()
            && output.as_ref().is_none_or(JoinHandle::is_finished)
            && diagnostics.as_ref().is_none_or(JoinHandle::is_finished)
        {
            if let Some(worker) = output.take() {
                let _ = join(worker);
            }
            if let Some(worker) = diagnostics.take() {
                let _ = join(worker);
            }
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("JPEG child/pipe cleanup exceeded original deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap, os::unix::fs::DirBuilderExt, path::PathBuf,
        sync::atomic::AtomicUsize,
    };
    const SUFFIX: &str = "a0/6716a80c98398cc0cb0a245f8e347fc804a84f";
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct TestTrace(PathBuf);
    impl TestTrace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "artwork-rendition-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestTrace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[derive(Default)]
    struct FakeReader {
        images: BTreeMap<u16, Vec<u8>>,
        stats: Vec<u16>,
        reads: Vec<u16>,
        failed_stat: Option<u16>,
        failed_read: Option<u16>,
        /// This edge's modification time changes on every stat.
        restless: Option<u16>,
    }
    impl DerivativeReader for FakeReader {
        async fn stat(&mut self, path: &str) -> Result<Option<Attributes>> {
            let (edge, _) = renditions::parse_cache_path(path)?;
            self.stats.push(edge);
            ensure!(
                self.failed_stat != Some(edge),
                "synthetic transport failure"
            );
            let mtime = if self.restless == Some(edge) {
                123 + self.stats.len()
            } else {
                123
            };
            Ok(self.images.get(&edge).map(|bytes| {
                Attributes::from([
                    ("st_ifmt".into(), "S_IFREG".into()),
                    ("st_size".into(), bytes.len().to_string()),
                    ("st_mtime".into(), mtime.to_string()),
                ])
            }))
        }
        async fn read_observed(
            &mut self,
            path: &str,
            local: &Path,
            before: Attributes,
        ) -> Result<(PermanentFile, File)> {
            let (edge, _) = renditions::parse_cache_path(path)?;
            self.reads.push(edge);
            ensure!(
                self.failed_read != Some(edge),
                "synthetic Present read failure"
            );
            let bytes = &self.images[&edge];
            private_write(local, bytes)?;
            Ok((
                PermanentFile {
                    afc_path: path.into(),
                    bytes: bytes.len() as u64,
                    sha256: hex(&Sha256::digest(bytes)),
                    after: before.clone(),
                    before,
                },
                File::open(local)?,
            ))
        }
    }
    fn control() -> Control {
        Control {
            deadline: Instant::now() + Duration::from_secs(15),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
    fn encoded_jpeg(edge: u16) -> Vec<u8> {
        encoded_jpeg_sized(edge, edge)
    }
    fn encoded_jpeg_sized(width: u16, height: u16) -> Vec<u8> {
        let trace = TestTrace::new();
        let raw = trace.0.join("pixels.rgb");
        private_write(
            &raw,
            &[40u8, 90, 170].repeat(usize::from(width) * usize::from(height)),
        )
        .unwrap();
        let result = Command::new("ffmpeg")
            .env_remove("FFREPORT")
            .args([
                "-nostdin",
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgb24",
                "-video_size",
                &format!("{width}x{height}"),
                "-i",
                "/proc/self/fd/0",
                "-frames:v",
                "1",
                "-threads",
                "1",
                "-c:v",
                "mjpeg",
                "-pix_fmt",
                "yuvj444p",
                "-f",
                "image2pipe",
                "pipe:1",
            ])
            .stdin(Stdio::from(File::open(raw).unwrap()))
            .output()
            .unwrap();
        assert!(result.status.success() && result.stderr.is_empty());
        assert!(result.stdout.len() < 64 * 1024);
        result.stdout
    }
    #[tokio::test]
    async fn missing_556_selects_present_531_and_decodes_without_visiting_later_paths() {
        let trace = TestTrace::new();
        let mut reader = FakeReader::default();
        reader.images.insert(531, encoded_jpeg(531));
        reader
            .images
            .insert(480, b"unvisited corrupt file".to_vec());
        let (proof, width, height, digest) =
            read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(reader.stats, [556, 531]);
        assert_eq!(reader.reads, [531]);
        assert_eq!((width, height), (531, 531));
        assert_eq!(renditions::rgb24_bytes(width, height).unwrap(), 845883);
        assert_eq!(
            proof.afc_path,
            renditions::derivative_path(SUFFIX, 531).unwrap()
        );
        assert!(digest.is_some_and(|d| d != [0; 32]));
        let first: serde_json::Value =
            serde_json::from_slice(&fs::read(trace.0.join("derivative-probe-556.json")).unwrap())
                .unwrap();
        assert_eq!(first["observation"], "unavailable_unconfirmed");
        assert_eq!(first["canonical_absence_claimed"], false);
        assert!(!trace.0.join("derivative-probe-480.json").exists());
    }
    #[tokio::test]
    async fn a_rendition_the_phone_left_empty_is_stat_twice_and_never_read() {
        let trace = TestTrace::new();
        let mut reader = FakeReader::default();
        reader.images.insert(480, Vec::new());
        let (file, width, height, digest) =
            read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(reader.stats, [556, 531, 480, 480]);
        assert!(reader.reads.is_empty());
        assert_eq!((width, height, digest), (480, 480, None));
        assert_eq!((file.bytes, file.sha256), (0, hex(&Sha256::digest([]))));
        assert_eq!(
            file.afc_path,
            renditions::derivative_path(SUFFIX, 480).unwrap()
        );
        assert!(!trace.0.join("derivative.jpg").exists());
        let trace = TestTrace::new();
        let mut reader = FakeReader {
            restless: Some(480),
            ..Default::default()
        };
        reader.images.insert(480, Vec::new());
        assert!(
            read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn the_556_rendition_is_read_first_and_exactly_decoded() {
        let trace = TestTrace::new();
        let mut reader = FakeReader::default();
        reader.images.insert(556, encoded_jpeg(556));
        reader.images.insert(531, b"must not inspect".to_vec());
        let (_, w, h, _) = read_derivative(&mut reader, SUFFIX, &trace.0, &control())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((w, h), (556, 556));
        assert_eq!(reader.stats, [556]);
        assert_eq!(reader.reads, [556]);
    }
    #[tokio::test]
    async fn a_rendition_of_a_cover_that_is_not_square_keeps_its_aspect() {
        // The reference MP3's 720x719 cover gave 556x555 under the 556 path.
        let trace = TestTrace::new();
        let mut reader = FakeReader::default();
        reader.images.insert(556, encoded_jpeg_sized(556, 555));
        let (_, w, h, digest) = read_derivative(&mut reader, SUFFIX, &trace.0, &control())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((w, h), (556, 555));
        assert!(digest.is_some());
        for wrong in [encoded_jpeg_sized(555, 554), encoded_jpeg_sized(557, 555)] {
            let trace = TestTrace::new();
            let mut reader = FakeReader::default();
            reader.images.insert(556, wrong);
            assert!(
                read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn present_read_geometry_truncation_and_entropy_failure_never_fall_back() {
        let smaller = encoded_jpeg(531);
        let mut truncated = encoded_jpeg(556);
        truncated.pop();
        for (bytes, fail_read) in [
            (jpeg(), true),
            (smaller.clone(), false),
            (truncated, false),
            (jpeg(), false),
        ] {
            let trace = TestTrace::new();
            let mut reader = FakeReader::default();
            reader.images.insert(556, bytes);
            reader.images.insert(531, smaller.clone());
            reader.failed_read = fail_read.then_some(556);
            assert!(
                read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                    .await
                    .is_err()
            );
            assert_eq!(reader.stats, [556]);
            assert_eq!(reader.reads, [556]);
        }
    }
    #[tokio::test]
    async fn transport_failure_cancel_and_exhausted_candidates_never_invent_success() {
        let trace = TestTrace::new();
        let mut reader = FakeReader {
            failed_stat: Some(556),
            ..Default::default()
        };
        assert!(
            read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                .await
                .is_err()
        );
        assert_eq!(reader.stats, [556]);
        assert!(reader.reads.is_empty());
        let trace = TestTrace::new();
        let mut reader = FakeReader::default();
        let cancelled = control();
        cancelled.cancelled.store(true, Ordering::Relaxed);
        assert!(
            read_derivative(&mut reader, SUFFIX, &trace.0, &cancelled)
                .await
                .is_err()
        );
        assert!(reader.stats.is_empty());
        // No rendition at all is no success either: the caller then requires
        // a cover smaller than every size (`without_rendition`).
        assert!(
            read_derivative(&mut reader, SUFFIX, &trace.0, &control())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(reader.stats, renditions::QUALIFIED_EDGES);
        assert!(reader.reads.is_empty());
        assert!(!trace.0.join("derivative.jpg").exists());
    }
    /// A 64x48 cover got no rendition at any size; a cover as
    /// large as the smallest qualified size must have one.
    #[test]
    fn only_a_cover_smaller_than_every_rendition_may_go_without_one() {
        for (width, height, accepted) in [
            (64, 48, true),
            (143, 100, true),
            (144, 10, false),
            (556, 556, false),
        ] {
            let trace = TestTrace::new();
            let bytes = encoded_jpeg_sized(width, height);
            let local = trace.0.join("original.jpg");
            fs::write(&local, &bytes).unwrap();
            let original = PermanentFile {
                afc_path: "Originals/aa/b".into(),
                bytes: bytes.len() as u64,
                sha256: hex(&Sha256::digest(&bytes)),
                before: Attributes::new(),
                after: Attributes::new(),
            };
            let mut held = File::open(&local).unwrap();
            assert_eq!(
                without_rendition(&original, &mut held).is_ok(),
                accepted,
                "{width}x{height}"
            );
        }
        // Bytes other than the read-back original never pass.
        let trace = TestTrace::new();
        let bytes = encoded_jpeg_sized(64, 48);
        let local = trace.0.join("original.jpg");
        fs::write(&local, &bytes).unwrap();
        let original = PermanentFile {
            afc_path: "Originals/aa/b".into(),
            bytes: bytes.len() as u64,
            sha256: "0".repeat(64),
            before: Attributes::new(),
            after: Attributes::new(),
        };
        assert!(without_rendition(&original, &mut File::open(&local).unwrap()).is_err());
    }
    fn cover() -> Cover {
        let bytes = b"synthetic cover".to_vec();
        Cover {
            sha256: Sha256::digest(&bytes).into(),
            checksum: bytes.len() as u32 + 17,
            bytes,
        }
    }
    fn original_expectation() -> OriginalArtworkExpectation {
        let cover = cover();
        OriginalArtworkExpectation {
            bytes: cover.bytes.len() as u64,
            sha256: cover.sha256,
        }
    }
    fn jpeg() -> Vec<u8> {
        let mut v = vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 2, 44, 2, 44, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0,
            0xff, 0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0,
        ];
        v.extend_from_slice(&[0x11, 0xff, 0, 0x22, 0xff, 0xd0, 0x33, 0xff, 0xd9]);
        v
    }
    #[test]
    fn exact_original_mismatch_is_rejected() {
        let cover = cover();
        let expected = original_expectation();
        let mut file = PermanentFile {
            afc_path: "unused".into(),
            bytes: cover.bytes.len() as u64,
            sha256: hex(&cover.sha256),
            before: Attributes::new(),
            after: Attributes::new(),
        };
        assert!(validate_original(&file, &expected).is_ok());
        file.bytes += 1;
        assert!(validate_original(&file, &expected).is_err());
        file.bytes -= 1;
        file.sha256 = "0".repeat(64);
        assert!(validate_original(&file, &expected).is_err());
    }
    #[test]
    fn track_identity_is_independent_of_shared_album_cache_and_cover() {
        let cover = cover();
        let expected = TrackArtworkExpectation {
            cache_id: 677,
            bytes: cover.bytes.len() as u64,
            sha256: cover.sha256,
        };
        let first = music_protocol::test_support::catalogue::artwork_catalog(677, 677);
        let sibling = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
        let a = track_plan(&first, u64::MAX, &expected).unwrap();
        let b = track_plan(&sibling, u64::MAX, &expected).unwrap();
        assert_eq!(a.relative_path, b.relative_path);
        assert_eq!(a.observed_album_cache_id, Some(677));
        assert_eq!(b.observed_album_cache_id, Some(678));
        // Only the track's bound bytes authorize this proof; another cover is
        // rejected even though its track and shared-album registrations exist.
        let original = PermanentFile {
            afc_path: a.relative_path,
            bytes: expected.bytes,
            sha256: hex(&expected.sha256),
            before: Attributes::new(),
            after: Attributes::new(),
        };
        assert!(validate_expected_original(&original, &expected).is_ok());
        assert!(
            validate_expected_original(
                &original,
                &TrackArtworkExpectation {
                    cache_id: 677,
                    bytes: expected.bytes,
                    sha256: [9; 32],
                }
            )
            .is_err()
        );
        for bad in [
            TrackArtworkExpectation {
                cache_id: 678,
                bytes: expected.bytes,
                sha256: expected.sha256,
            },
            TrackArtworkExpectation {
                cache_id: 677,
                bytes: 0,
                sha256: expected.sha256,
            },
            TrackArtworkExpectation {
                cache_id: 677,
                bytes: MAX_IMAGE_BYTES + 1,
                sha256: expected.sha256,
            },
            TrackArtworkExpectation {
                cache_id: 677,
                bytes: expected.bytes,
                sha256: [0; 32],
            },
        ] {
            assert!(track_plan(&sibling, u64::MAX, &bad).is_err());
        }
    }
    #[test]
    fn a_cover_no_album_uses_is_read_back_like_an_album_cover() {
        let cover = cover();
        let expected = TrackArtworkExpectation {
            cache_id: 677,
            bytes: cover.bytes.len() as u64,
            sha256: cover.sha256,
        };
        let item = music_protocol::test_support::catalogue::artwork_catalog_typed(677, 678, 1);
        let selected = track_plan(&item, u64::MAX, &expected).unwrap();
        assert_eq!(selected.observed_album_cache_id, Some(678));
        let other = music_protocol::test_support::catalogue::artwork_catalog_typed(677, 678, 4);
        assert!(track_plan(&other, u64::MAX, &expected).is_err());
    }
    #[test]
    fn album_original_plan_does_not_require_any_track_to_use_album_cache() {
        let catalog = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
        let album_id = catalog.tracks()[0].album_sync_id;
        assert!(
            !catalog
                .artwork()
                .track_cache_ids()
                .values()
                .any(|id| *id == 678)
        );
        let selected = album_plan(&catalog, album_id, &original_expectation()).unwrap();
        assert_eq!(selected.cache_id, 678);
        assert_eq!(
            selected.relative_path,
            "aa/00000000000000000000000000000000000000"
        );
        assert!(album_plan(&catalog, 0, &original_expectation()).is_err());
        assert!(album_plan(&catalog, 999, &original_expectation()).is_err());
        let absent = music_protocol::test_support::catalogue::collection_catalog(13);
        assert!(album_plan(&absent, album_id, &original_expectation()).is_err());
        let mut invalid = cover();
        invalid.checksum += 1;
        assert!(validate_cover(&invalid).is_err());
    }
    #[test]
    fn marker_qualification_is_bounded_not_entropy_proof() {
        let good = jpeg();
        assert_eq!(jpeg_dimensions(&good).unwrap(), (556, 556));
        for end in 0..good.len() {
            assert!(jpeg_dimensions(&good[..end]).is_err());
        }
        let mut bad = good.clone();
        bad.extend_from_slice(&good);
        assert!(jpeg_dimensions(&bad).is_err());
        let mut bad = good.clone();
        bad[3] = 0xc2;
        assert!(jpeg_dimensions(&bad).is_err());
        let mut bad = good;
        bad[6] = 16;
        assert!(jpeg_dimensions(&bad).is_err());
    }

    #[tokio::test]
    async fn unknown_cache_and_invalid_cover_refuse_before_connector() {
        let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
        let mut prefs = vec![0u8; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
        prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
        prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
        let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
        let identity =
            crate::engine::state::journal::Identity::new("synthetic-device", &library).unwrap();
        let current = Current {
            device: "synthetic-device".into(),
            identity,
            library,
            catalog,
            raw_preferences: prefs.clone(),
            plist_preferences: prefs,
            cdb: vec![],
            artwork_db: None,
            receipt_sha256: [1; 32],
        };
        let control = Control {
            deadline: Instant::now() + Duration::from_secs(10),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let called = AtomicBool::new(false);
        let connect = || async {
            called.store(true, Ordering::Relaxed);
            bail!("unexpected connector")
        };
        assert!(
            verify_track(
                &current,
                u64::MAX,
                677,
                &cover(),
                Path::new("/invalid-not-created"),
                &control,
                connect
            )
            .await
            .is_err()
        );
        assert!(!called.load(Ordering::Relaxed));
        assert!(
            verify_track_expected(
                &current,
                u64::MAX,
                &TrackArtworkExpectation {
                    cache_id: 677,
                    bytes: 0,
                    sha256: [9; 32],
                },
                Path::new("/invalid-not-created"),
                &control,
                connect
            )
            .await
            .is_err()
        );
        assert!(!called.load(Ordering::Relaxed));
        assert!(
            verify_album_original(
                &current,
                current.catalog.tracks()[0].album_sync_id,
                &cover(),
                Path::new("/invalid-not-created"),
                &control,
                connect,
            )
            .await
            .is_err()
        );
        assert!(!called.load(Ordering::Relaxed));
    }
    #[test]
    fn cover_groups_fill_one_connection_operation_bound() {
        // Both guards of a 5 MiB state, as on the phone: 80 read chunks each.
        let state = 80;
        let small = 187_306;
        let fits = (MAX_BOUNDED_OPS as u64 - 2 * state - GUARD_EXTRA_OPS) / track_ops(small);
        assert!(fits > MAX_GROUP as u64);
        let originals = vec![small; 100];
        assert_eq!(group_len(state, originals.iter().copied()), MAX_GROUP);
        assert_eq!(
            group_len(state, originals[MAX_GROUP..].iter().copied()),
            100 - MAX_GROUP
        );
        assert_eq!(group_len(state, []), 0);
        // Maximum originals still group, larger ones fill the operation
        // bound first, and an unbounded one never fits.
        assert_eq!(group_len(state, [MAX_IMAGE_BYTES; 10]), 10);
        let third = MAX_BOUNDED_OPS as u64 / 3 * REQUEST;
        assert_eq!(group_len(state, [third; 4]), 2);
        assert_eq!(group_len(state, [MAX_BOUNDED_OPS as u64 * REQUEST]), 0);
        assert_eq!(group_len(u64::MAX, [small]), 0);
    }
    #[test]
    fn bounded_decoder_checks_output_logs_exit_timeout_and_cancellation() {
        let run = |script: &str, expected: u64| {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            supervise(
                &mut command,
                move |output| output_hash(output, expected),
                std::time::Instant::now() + Duration::from_secs(3),
                Arc::new(AtomicBool::new(false)),
            )
        };
        assert_eq!(
            run("printf abc", 3).unwrap(),
            <[u8; 32]>::from(Sha256::digest(b"abc"))
        );
        for (script, size) in [
            ("printf ab", 3),
            ("printf abcd", 3),
            ("printf abc; printf warning >&2", 3),
            ("printf abc; exit 1", 3),
            ("exec sleep 3", 3),
        ] {
            assert!(run(script, size).is_err());
        }
        let mut command = Command::new("/not-called");
        assert!(
            supervise(
                &mut command,
                |output| output_hash(output, 3),
                std::time::Instant::now() + Duration::from_secs(3),
                Arc::new(AtomicBool::new(true))
            )
            .is_err()
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancel);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            trigger.store(true, Ordering::Relaxed);
        });
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 10"]);
        let started = std::time::Instant::now();
        assert!(
            supervise(
                &mut command,
                |output| output_hash(output, 3),
                started + Duration::from_secs(3),
                cancel
            )
            .is_err()
        );
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(output_hash(&b"abc"[..], 2).is_err());
        assert!(drain_log(&vec![b'x'; LOG_CAP + 1][..]).unwrap().truncated);
    }
    /// A rendition held in a local file, as a group's read leaves it.
    fn held(trace: &TestTrace, name: &str, bytes: &[u8], edge: u16) -> HeldRendition {
        let path = trace.0.join(name);
        private_write(&path, bytes).unwrap();
        HeldRendition {
            held: File::open(path).unwrap(),
            edge,
            bytes: bytes.len() as u64,
            sha256: Sha256::digest(bytes).into(),
        }
    }
    #[test]
    fn a_group_decodes_in_one_process_exactly_as_each_cover_alone() {
        let trace = TestTrace::new();
        let images = [
            (encoded_jpeg(556), 556),
            (encoded_jpeg_sized(556, 555), 556),
            (encoded_jpeg(531), 531),
        ];
        let group: Vec<_> = images
            .iter()
            .enumerate()
            .map(|(i, (bytes, edge))| held(&trace, &format!("{i}.jpg"), bytes, *edge))
            .collect();
        let together = decode_batch(&group, &control()).unwrap();
        for (r, decoded) in group.into_iter().zip(together) {
            let alone =
                verify_held_rendition(r.held, r.edge, r.bytes, r.sha256, &control()).unwrap();
            assert_eq!(
                (alone.width, alone.height, alone.rgb24_sha256),
                (decoded.width, decoded.height, decoded.rgb24_sha256)
            );
        }
        // A cover that does not decode fails only its own check.
        let group = vec![
            held(&trace, "a.jpg", &images[0].0, 556),
            held(&trace, "bad.jpg", &jpeg(), 556),
            held(&trace, "c.jpg", &images[2].0, 531),
        ];
        let results = verify_held_renditions(group, &control());
        assert!(results[0].is_ok() && results[1].is_err() && results[2].is_ok());
        let line = format!("0, 0, 0, 1, 3, {}\n", "ab".repeat(32));
        assert_eq!(frame_hashes(line.as_bytes(), 1).unwrap(), [(3, [0xab; 32])]);
        assert!(frame_hashes(line.as_bytes(), 2).is_err());
        assert!(frame_hashes(line.replace(", 3,", ", x,").as_bytes(), 1).is_err());
    }
}

#[cfg(test)]
mod source_tests;
