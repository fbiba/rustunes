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

/// Operations a reuse attempt may take: a stat of the original, a probe of
/// each qualified rendition size, and a stat of both files after.
const REUSE_OPS: u64 = 1 + renditions::QUALIFIED_EDGES.len() as u64 + 2;
/// Covers per reuse connection at most; each takes a few stats.
const MAX_REUSE_GROUP: usize = 512;

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

/// The decoded raster's size; zero when the phone left the rendition empty.
fn rgb24_bytes(decoded: Option<[u8; 32]>, width: u16, height: u16) -> Result<u64> {
    match decoded {
        Some(_) => renditions::rgb24_bytes(width, height),
        None => Ok(0),
    }
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
