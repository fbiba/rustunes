//! One signed native session that inserts several tracks, ALAC or MP3, one
//! `update_db_info` and one `insert_track` per track, journaled as an
//! insert batch. Each codec's insert comes from its own encoder.
//!
//! Members are planned together from one fresh complete state, so tracks of a
//! new album share its new album and artist. The batch is Prepared before any
//! connection, Started only after the live session is Ready, every source is
//! held unchanged and the operation is signed. The phone then names every new
//! track in one asset manifest; each file is uploaded, read back, renamed and
//! completed in reverse operation order. A member with a cover names it in
//! its insert, as reference iTunes does; the phone then asks for its audio and
//! artwork together, and the cover is staged before the audio. Settlement is
//! by observation: every planned row and field (`verify_insert_batch_covered`),
//! prior tracks and files exact, each permanent file holding the exact source
//! bytes and each permanent cover the exact source image. A session that provably
//! did not apply becomes NotApplied. Rows the phone created without their
//! audio make the batch Partial until a compensating removal batch removes
//! them. Rows the phone created and deleted again (it drops the rows in
//! flight when a session is abandoned) need nothing more once no file exists
//! at their planned paths. Nothing is ever resent.
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Seek},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{Instant, timeout_at};

use crate::{
    engine::execution::device,
    engine::execution::native_artwork,
    engine::execution::native_insert::{
        BoundSource, Control, FreshState, create_trace, durable_write, sync_trace,
    },
    engine::planning::collections::{CandidateIds, Planner},
    engine::planning::source_collection::mp3::preparation::PreparedSource,
    engine::recovery::retained::playback::{Window, between_runs},
    engine::state::audio_index::Absent,
    engine::state::current,
    engine::state::journal::{
        Artwork, BatchRecord, BatchState, CompletionEvidence, Content, CoordinationId, Identity,
        InsertBatchId, InsertBatchRecord, InsertBatchState, InsertIntent, InsertTarget, Journal,
        RemovalIntent, SessionIds, TargetOutcome,
    },
    engine::state::snapshot::{self, Snapshot},
    engine::verification::artwork::{TrackArtworkExpectation, TrackCheck},
    engine::{
        execution::{
            cover, flac_to_alac, insert,
            removal_batch::{self, Before, controlled, hex, next_directory, sha},
        },
        recovery::stale_staging,
        verification::removal_sql,
    },
};
use music_protocol::catalogue::insert_verification::{self, ExpectedInsert, ExpectedSource};
use music_protocol::catalogue::mutation;
use music_protocol::media::artwork::{self, Cover};
use music_protocol::media::fingerprint::{SourceFingerprint, TransferredSource};
use music_protocol::media::metadata::{self, Metadata};
use music_protocol::protocol::session::{self, Session};
use music_protocol::protocol::templates;
use music_protocol::protocol::transfer::{MAX_ASSET, MAX_SESSION_OPS, Transfer};

const AIRLOCK: &str = "Airlock/Media";
const ARTWORK: &str = "Airlock/Media/Artwork";
const ARTWORK_DB: &str = "iTunes_Control/Artwork/ArtworkDB";
const CHUNK: u64 = 64 * 1024;
const STAGING: &str = "iTunes_Control/Sync/Media";

/// One planned addition: the validated source and the saved plan's
/// identities and permanent path.
pub(crate) struct Member {
    /// Relative to the source root, for receipts and progress.
    pub(crate) relative_path: String,
    pub(crate) source: PathBuf,
    pub(crate) audio: Audio,
    pub(crate) track: u64,
    pub(crate) album: u64,
    pub(crate) artist: u64,
    pub(crate) destination: String,
    /// The source's embedded cover, sent with its audio. The caller clears it
    /// for a track whose album image must stay as it is, unless it sets
    /// `keep_album_image`.
    pub(crate) cover: Option<Cover>,
    /// The album was on the phone and must keep its image (add-only): the
    /// cover rides only when the album's permanent original holds exactly
    /// its bytes, read back after each batch's fresh capture. Otherwise the
    /// batch leaves it out.
    pub(crate) keep_album_image: bool,
    /// The FLAC `source` was converted from (`--flac-to-alac`), recorded
    /// with the batch's target.
    pub(crate) conversion: Option<crate::engine::state::journal::Conversion>,
    /// `source` is a FLAC (`audio` its reading) that goes as the ALAC
    /// converted from it; `add_all` converts it ahead of its batch.
    pub(crate) convert: bool,
}

/// A member's source, read again from its file after planning.
pub(crate) enum Audio {
    /// An ALAC M4A: its fingerprint and tags.
    Alac {
        fingerprint: SourceFingerprint,
        metadata: Metadata,
    },
    /// Any other format (MP3, AAC, FLAC, WAV, AIFF) in the insert-batch
    /// profile; shared with a FLAC's conversion.
    Mpeg(Arc<PreparedSource>),
}

impl Audio {
    fn transferred(&self) -> TransferredSource {
        match self {
            Self::Alac { fingerprint, .. } => fingerprint.clone().into(),
            Self::Mpeg(source) => source.identity().clone(),
        }
    }
    /// What the member's catalogue row must say.
    fn expected(&self) -> Result<ExpectedSource<'_>> {
        match self {
            Self::Alac { metadata, .. } => Ok(metadata.into()),
            Self::Mpeg(source) => source.expected(),
        }
    }
    /// Hold the file: the operation binds these exact bytes.
    fn open(&self, path: &Path) -> Result<BoundSource> {
        match self {
            Self::Alac { fingerprint, .. } => BoundSource::open(path, fingerprint),
            Self::Mpeg(source) => {
                let identity = source.identity();
                BoundSource::open_observed(path, identity.file_bytes, identity.full_file_sha256)
            }
        }
    }
    /// Read `path` again as the source a batch target recorded.
    fn reread(path: &Path, sent: &TransferredSource) -> Result<Self> {
        if sent.is_alac() {
            let fingerprint = music_protocol::media::fingerprint::read(path)?;
            ensure!(
                fingerprint.full_file_sha256 == sent.full_file_sha256,
                "it changed since the batch was sent"
            );
            return Ok(Self::Alac {
                fingerprint,
                metadata: metadata::read(path)?,
            });
        }
        let source =
            PreparedSource::read_expected(path, &music_protocol::media::mpeg::read(path)?)?;
        ensure!(
            source.identity() == sent,
            "it changed since the batch was sent"
        );
        Ok(Self::Mpeg(Arc::new(source)))
    }
}

/// The staging name's extension, by format.
fn suffix(source: &TransferredSource) -> &'static str {
    source.format().extension()
}

impl Member {
    fn asset_id(&self) -> String {
        (self.track as i64).to_string()
    }
    fn stage(&self) -> String {
        format!(
            "{AIRLOCK}/{}{}",
            self.asset_id(),
            suffix(&self.audio.transferred())
        )
    }
    fn bare(&self) -> String {
        format!("{AIRLOCK}/{}", self.asset_id())
    }
    fn artwork_stage(&self) -> String {
        format!("{ARTWORK}/{}", self.asset_id())
    }
    /// The source its binding records: a converted member's FLAC, as for a
    /// single insert, since the ALAC it sends is removed once its batch
    /// settles.
    fn recorded_source(&self) -> PathBuf {
        self.conversion
            .as_ref()
            .map_or_else(|| self.source.clone(), |c| c.source_path.clone())
    }
}

/// The new tracks of a saved plan that go in insert batches, in plan order,
/// with the plan's collection scope and the number of other rows (audio
/// replacements, new tracks the batches leave out, and conflicts), which the
/// fresh sync afterwards takes one per session. With a batch size above one
/// every new ALAC track and MP3 is a member; with one, only the MP3s, which no
/// one-track session sends. A re-add that reuses a retired track's groups is
/// an ordinary member: the batch reuses every existing group by its ID.
pub(crate) struct Batchable {
    pub(crate) members: Vec<Member>,
    pub(crate) scope: [u8; 32],
    pub(crate) others: usize,
}

pub(crate) fn members(
    checked: &crate::engine::planning::saved_plan::Validated,
    batch_size: usize,
) -> Result<Batchable> {
    use crate::engine::planning::additive::Decision;
    let mut members = Vec::new();
    let mut others = 0;
    for row in &checked.plan.sources {
        // A skipped source is left out, whatever else it describes.
        if matches!(row.decision, Decision::Skip { .. }) {
            continue;
        }
        if row.managed_audio_replacement.is_some() {
            others += 1;
            continue;
        }
        match &row.decision {
            Decision::Add {
                track_id,
                album,
                artist,
                path: destination,
                ..
            } => {
                let path = checked.sources.base_directory.join(&row.relative_path);
                let alac = checked
                    .sources
                    .tracks
                    .iter()
                    .find(|s| s.relative_path == row.relative_path);
                let mpeg = checked
                    .sources
                    .mp3_tracks
                    .iter()
                    .find(|s| s.relative_path == row.relative_path);
                // Read again from the same file; holding the source binds it
                // to these bytes before anything is sent.
                let (audio, cover) = match (alac, mpeg) {
                    (Some(source), _) if batch_size > 1 => {
                        let metadata = metadata::read(&path)?;
                        let art = artwork::read(&path)?;
                        ensure!(
                            metadata == source.metadata
                                && art.source_size == source.fingerprint.file_bytes
                                && art.source_sha256 == source.fingerprint.full_file_sha256,
                            "{} changed after planning",
                            source.relative_path
                        );
                        let fingerprint = source.fingerprint.clone();
                        (
                            Audio::Alac {
                                fingerprint,
                                metadata,
                            },
                            art.cover,
                        )
                    }
                    (Some(_), _) if row.flac_to_alac => {
                        bail!("{} is not a FLAC to convert", row.relative_path)
                    }
                    (None, Some(source)) => {
                        let prepared = PreparedSource::read_expected(&path, &source.source)
                            .with_context(|| {
                                format!("{} changed after planning", source.relative_path)
                            })?;
                        prepared.ensure_batch_insertion_allowed()?;
                        let cover =
                            crate::engine::planning::source_collection::mp3::artwork::batch_cover(
                                &path,
                                prepared.source(),
                            )?;
                        (Audio::Mpeg(Arc::new(prepared)), cover)
                    }
                    _ => {
                        others += 1;
                        continue;
                    }
                };
                members.push(Member {
                    relative_path: row.relative_path.clone(),
                    source: path,
                    audio,
                    track: u64::from_str_radix(track_id, 16)?,
                    album: u64::from_str_radix(&album.id, 16)?,
                    artist: u64::from_str_radix(&artist.id, 16)?,
                    destination: destination.clone(),
                    cover,
                    keep_album_image: false,
                    conversion: None,
                    convert: row.flac_to_alac,
                });
            }
            // Sync sends metadata updates after the batches.
            Decision::Keep | Decision::Update { .. } => {}
            _ => others += 1,
        }
    }
    let scope = crate::engine::planning::removal::collection_scope_sha256(&checked.sources)?;
    Ok(Batchable {
        members,
        scope,
        others,
    })
}

/// The source files the outstanding insert batch's input names, which its
/// settlement reads again.
pub(crate) fn outstanding_sources(journal: &Journal) -> Result<Vec<PathBuf>> {
    let Some(batch) = journal.outstanding_insert_batch()? else {
        return Ok(Vec::new());
    };
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(batch.evidence.join("input.json"))?)?;
    input["members"]
        .as_array()
        .context("batch input has no members")?
        .iter()
        .map(|m| {
            m["source_path"]
                .as_str()
                .map(PathBuf::from)
                .context("batch member has no source path")
        })
        .collect()
}

/// What a batch ended as.
pub(crate) struct Settled {
    pub(crate) batch: InsertBatchId,
    pub(crate) state: InsertBatchState,
    pub(crate) evidence: PathBuf,
    pub(crate) inserted: usize,
    /// Tracks a session's removal half removed.
    pub(crate) removed: usize,
}

/// What one target's row must be: its source's tags and the groups the plan
/// put it in.
struct Planned<'a> {
    source: ExpectedSource<'a>,
    album: u64,
    artist: u64,
}

/// The combined operation and journal targets for `members`, planned against
/// the exact `before` state. Each member must group exactly as the saved plan
/// did; collections an earlier member creates are shared, never re-created.
pub(crate) fn plan(
    before: &Before,
    journal: &Journal,
    members: &[Member],
    collection: [u8; 32],
    imported_at: SystemTime,
) -> Result<(Vec<u8>, Vec<InsertTarget>)> {
    ensure!(
        members.iter().all(|m| !m.convert),
        "a FLAC must be converted before its batch is planned"
    );
    let catalog = &before.current.catalog;
    let mut planner = Planner::new(catalog, &journal.reserved_pids()?);
    // Cover cache IDs above every saved, registered and reserved one, as for a
    // single cover, then one after another.
    let mut next_cache = if members.iter().any(|m| m.cover.is_some()) {
        let saved = before
            .current
            .artwork_db
            .as_deref()
            .context("artwork database capture absent")?;
        Some(native_artwork::allocate_cache_id(catalog, saved, journal)?)
    } else {
        None
    };
    let mut created = BTreeSet::new();
    let mut singles = Vec::with_capacity(members.len());
    let mut targets = Vec::with_capacity(members.len());
    for member in members {
        let ids = CandidateIds {
            track: member.track,
            album: Some(member.album),
            artist: Some(member.artist),
        };
        let selected = match &member.audio {
            Audio::Alac { metadata, .. } => planner.plan(metadata, ids)?,
            Audio::Mpeg(source) => planner.plan_mpeg(source, ids)?,
        };
        ensure!(
            selected.track_pid() == member.track
                && selected.album_pid() == member.album
                && selected.artist_pid() == member.artist,
            "fresh grouping of {} differs from the saved plan; no alternate IDs will be substituted",
            member.relative_path
        );
        let single = match &member.audio {
            Audio::Alac { metadata, .. } => {
                insert::prepare_batch_member(metadata, &selected, catalog, imported_at, &created)?
            }
            Audio::Mpeg(source) => insert::prepare_batch_mpeg_member(
                source,
                &selected,
                catalog,
                imported_at,
                &created,
            )?,
        };
        let artwork = match (&member.cover, next_cache.as_mut()) {
            (Some(cover), Some(next)) => {
                let cache_id = *next;
                *next = next
                    .checked_add(1)
                    .context("artwork cache namespace exhausted")?;
                Some((cover, cache_id))
            }
            _ => None,
        };
        singles.push(match artwork {
            Some((cover, cache_id)) => {
                music_protocol::protocol::batch::with_artwork(&single, cache_id, cover.checksum)?
            }
            None => single,
        });
        created.extend(selected.new_collection_ids().iter().copied());
        let new = |id: u64| selected.new_collection_ids().contains(&id).then_some(id);
        targets.push(InsertTarget {
            content: Content {
                source: member.audio.transferred(),
                track_pid: member.track,
                path: member.destination.clone(),
                artwork: artwork.map(|(cover, cache_id)| Artwork {
                    cache_id,
                    bytes: cover.bytes.len() as u64,
                    sha256: cover.sha256,
                }),
                collection: Some(collection),
            },
            new_album_pid: new(member.album),
            new_artist_pid: new(member.artist),
            conversion: member.conversion.clone(),
            source: Some(member.recorded_source()),
        });
    }
    Ok((music_protocol::protocol::batch::combine(&singles)?, targets))
}

/// Hold every member's source: the operation binds these bytes.
pub(crate) fn hold(members: &[Member]) -> Result<Vec<BoundSource>> {
    members.iter().map(|m| m.audio.open(&m.source)).collect()
}

/// A session's offline checks against the exact `before` state, kept for its
/// send: the bound state, the prepared session and each connection's
/// operation allowance.
pub(crate) struct Checked {
    state: FreshState,
    session: session::Prepared,
    guard_operations: usize,
    upload_connections: Vec<Option<usize>>,
    artwork_db: Option<(u64, [u8; 32])>,
}

/// The phone asks for each covered member's audio and artwork together.
fn audio_policy<'a>(
    state: &'a FreshState,
    members: &[Member],
) -> Result<music_protocol::protocol::ancillary::AudioPolicy<'a>> {
    let assets: Vec<(String, bool)> = members
        .iter()
        .map(|m| (m.asset_id(), m.cover.is_some()))
        .collect();
    state.ancillary().audio_batch_with_artwork(&assets)
}

/// How far `send_session` got.
pub(crate) enum Sent {
    /// Nothing was written to the phone; the caller cancels its Prepared record.
    Unsent(anyhow::Error),
    /// Started was recorded; the session's receipts and uploads, or why it
    /// failed. Only observation settles it.
    Started(Result<Wire>),
}

/// A finished session's receipts and each member's upload, in upload order.
pub(crate) struct Wire {
    pub(crate) ready: [u8; 32],
    pub(crate) terminal: [u8; 32],
    pub(crate) operation: [u8; 32],
    pub(crate) uploaded: Vec<serde_json::Value>,
}

/// Connect, guard and sign the `checked` operation, then call `start`, which
/// must durably record Started and return the journal's copy of the bytes,
/// and send them: stage the metadata, then upload, rename and complete each
/// member's cover and audio. The guards hold the state files, every member's
/// names and source, and the ArtworkDB when checked, unchanged through the
/// Ready session. Traces go under `evidence` (`send`, `upload-<asset>`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_session<T: session::StartedOperation>(
    before: &Before,
    identity: &Identity,
    members: &[Member],
    held: &mut [BoundSource],
    checked: Checked,
    operation: &[u8],
    evidence: &Path,
    control: &Control,
    start: impl FnOnce([u8; 32], [u8; 21]) -> Result<T>,
) -> Sent {
    let current = &before.current;
    let Checked {
        state,
        session: session_prepared,
        guard_operations,
        upload_connections,
        artwork_db,
    } = checked;
    let send = evidence.join("send");
    let policy = match audio_policy(&state, members).and_then(|policy| {
        create_trace(&send)?;
        Ok(policy)
    }) {
        Ok(policy) => policy,
        Err(error) => return Sent::Unsent(error),
    };
    let covered: Vec<&Member> = members.iter().filter(|m| m.cover.is_some()).collect();
    let names: Vec<_> = members
        .iter()
        .map(|m| (m.stage(), m.bare(), m.destination.clone()))
        .collect();
    let mut session = None;
    let mut afc = None;
    let mut provider = None;
    let ready = controlled(control, async {
        let found = timeout_at(
            control
                .deadline
                .min(Instant::now() + Duration::from_secs(15)),
            device::provider(),
        )
        .await
        .context("insertion discovery deadline")??;
        ensure!(
            found.udid == current.device,
            "discovered device differs from the captured state"
        );
        let found = provider.insert(found);
        let afc = afc.insert(
            Transfer::connect_bounded(
                found,
                &send,
                control.deadline,
                guard_operations,
                Arc::clone(&control.cancelled),
            )
            .await?,
        );
        crate::engine::execution::native_insert::batch_guard(afc, &state, &names).await?;
        artwork_guard(afc, &covered, artwork_db).await?;
        for source in held.iter() {
            source.check()?;
        }
        control.check()?;
        let live = session
            .insert(Session::connect(found, session_prepared, &send, control.deadline).await?);
        crate::engine::execution::native_insert::batch_guard(afc, &state, &names).await?;
        artwork_guard(afc, &covered, artwork_db).await?;
        for source in held.iter() {
            source.check()?;
        }
        let signature = live.sign_metadata(operation)?;
        durable_write(&send, "operation.cig", &signature)?;
        sync_trace(&send)?;
        Ok((live.ready_receipt_sha256(), signature))
    })
    .await
    .and_then(|(ready, signature)| {
        control.check()?;
        Ok((ready, start(ready, signature)?))
    });
    let (ready, started) = match ready {
        Ok(ready) => ready,
        Err(error) => return Sent::Unsent(error),
    };
    let (Some(mut session), Some(mut afc), Some(provider)) = (session, afc, provider) else {
        return Sent::Started(Err(anyhow::anyhow!(
            "insertion transport lost before Started"
        )));
    };
    let wire = controlled(control, async {
        session.stage_metadata(&mut afc, &started).await?;
        control.check()?;
        session.finish_metadata().await?;
        session.wait_for_audio_with_policy(&policy).await?;
        crate::engine::execution::native_insert::staging_folders(&mut afc, !covered.is_empty())
            .await?;
        drop(afc);
        let mut uploaded = Vec::with_capacity(members.len());
        let mut upload: Option<(Transfer, PathBuf)> = None;
        // In reverse operation order: the phone gives each album the cover of
        // its last member in the operation, so an interrupted batch leaves
        // that cover present for every album any of whose members arrived.
        for (index, (member, source)) in members.iter().zip(held.iter_mut()).enumerate().rev() {
            // A connection carries uploads while its operation allowance
            // covers them, and is named after its first file. Its trace sits
            // beside `send`, which holds only regular files.
            if let Some(operations) = upload_connections[index] {
                let trace = evidence.join(format!("upload-{}", member.asset_id()));
                create_trace(&trace)?;
                let mut afc = Transfer::connect_bounded(
                    &provider,
                    &trace,
                    control.deadline,
                    operations,
                    Arc::clone(&control.cancelled),
                )
                .await?;
                session.permit_writes(&mut afc, &started)?;
                upload = Some((afc, trace));
            }
            let (afc, trace) = upload.as_mut().context("upload connection absent")?;
            let (stage, bare) = (member.stage(), member.bare());
            source.check()?;
            // The cover goes first: the completion below claims both parts.
            if let Some(cover) = &member.cover {
                let stage = member.artwork_stage();
                afc.require_absent(&stage).await?;
                let bytes = cover.bytes.len() as u64;
                let sent = afc
                    .write_new(&stage, &mut Cursor::new(&cover.bytes), bytes)
                    .await?;
                ensure!(
                    sent == cover.sha256,
                    "staged cover differs for {}; stage retained",
                    member.relative_path
                );
            }
            source.file().rewind()?;
            let bytes = source.file_bytes();
            let mut upload = crate::engine::execution::native_insert::ObservedUpload::new(
                source.file(),
                Some(&member.relative_path),
                member.track,
                bytes,
            );
            let sent = afc.write_audio_new(&stage, &mut upload, bytes).await?;
            upload.finish()?;
            ensure!(
                sent == member.audio.transferred().full_file_sha256,
                "uploaded source hash differs for {}; partial stage retained",
                member.relative_path
            );
            source.check()?;
            afc.require_absent(&member.destination).await?;
            let renamed = afc.rename_absent(&stage, &bare, bytes).await?;
            crate::engine::state::audio_index::cache::uploaded(
                &identity.device_digest(),
                &member.destination,
                &renamed,
                &member.audio.transferred(),
            );
            afc.require_absent(&member.destination).await?;
            control.check()?;
            // Each format's completion checks its own permanent extension.
            match &member.audio {
                Audio::Alac { .. } => {
                    session
                        .complete_audio(&member.asset_id(), &member.destination)
                        .await?
                }
                Audio::Mpeg(source) => {
                    session
                        .complete_format_audio(
                            &member.asset_id(),
                            &member.destination,
                            source.source().format(),
                        )
                        .await?
                }
            }
            uploaded.push(
                json!({"track":format!("{:016x}",member.track),"sha256":hex(&sent),
                "cover_sha256":member.cover.as_ref().map(|c| hex(&c.sha256))}),
            );
            sync_trace(trace)?;
        }
        drop(upload);
        session.wait_finished().await?;
        Ok(Wire {
            ready,
            terminal: session.terminal_receipt_sha256()?,
            operation: sha(started.operation()),
            uploaded,
        })
    })
    .await;
    drop(session);
    Sent::Started(wire)
}

/// With covers: the saved ArtworkDB the cache IDs were allocated from is
/// unchanged, and no member's cover stage exists yet.
async fn artwork_guard(
    afc: &mut Transfer,
    covered: &[&Member],
    saved: Option<(u64, [u8; 32])>,
) -> Result<()> {
    let Some((bytes, digest)) = saved else {
        return Ok(());
    };
    ensure!(
        afc.read_hash(ARTWORK_DB, bytes, MAX_ASSET).await? == digest,
        "the phone's ArtworkDB changed since the cover cache IDs were allocated"
    );
    afc.require_directory_or_absent(ARTWORK).await?;
    for member in covered {
        afc.require_absent(&member.artwork_stage()).await?;
    }
    Ok(())
}

/// A batch was cut short and the rows it left without audio were taken out
/// again: the phone's library is consistent, and syncing again adds the rest.
/// The count is the tracks the run's sessions removed.
#[derive(Debug)]
pub(crate) struct CutShort(pub(crate) String, pub(crate) usize);
impl std::fmt::Display for CutShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; run sync again for the rest", self.0)
    }
}
impl std::error::Error for CutShort {}

/// The rows a Partial batch left without audio, in batch order.
pub(crate) fn unfilled(batch: &InsertBatchRecord) -> Vec<u64> {
    batch
        .targets
        .iter()
        .filter(|t| t.outcome == Some(TargetOutcome::PendingCompensation))
        .map(|t| t.target.content.track_pid)
        .collect()
}

/// Remove the rows a Partial batch left without audio in one compensating
/// removal batch (evidence under the batch's `compensate-NNN`). Verified, it
/// makes the insert batch Compensated; the tracks it did bind stay.
pub(crate) async fn compensate(
    journal_path: &Path,
    identity: &Identity,
    batch: &InsertBatchRecord,
    host_name: &str,
    control: &Control,
) -> Result<removal_batch::Settled> {
    ensure!(
        batch.state == InsertBatchState::Partial,
        "insert batch {} is not Partial",
        batch.id.as_i64()
    );
    let tracks = unfilled(batch);
    let planned: Vec<String> = batch
        .targets
        .iter()
        .filter(|t| tracks.contains(&t.target.content.track_pid))
        .map(|t| t.target.content.path.clone())
        .collect();
    removal_batch::compensate(
        journal_path,
        identity,
        &next_directory(&batch.evidence, "compensate")?,
        &tracks,
        &planned,
        removal_batch::Purpose::CompensateInsertBatch(batch.id),
        batch.coordination,
        host_name,
        control,
    )
    .await
}

/// Each member's source path, album and artist, as the batch's retained input
/// holds them.
fn retained_members(batch: &InsertBatchRecord) -> Result<Vec<(PathBuf, u64, u64)>> {
    let input = std::fs::read(batch.evidence.join("input.json"))?;
    ensure!(
        sha(&input) == batch.input_receipt,
        "retained batch input differs from the journal"
    );
    let input: serde_json::Value = serde_json::from_slice(&input)?;
    let id = |m: &serde_json::Value, key: &str| -> Result<u64> {
        let text = m[key].as_str().context("batch member group")?;
        ensure!(text.len() == 16, "batch member group");
        Ok(u64::from_str_radix(text, 16)?)
    };
    let members = input["members"]
        .as_array()
        .context("batch input has no members")?
        .iter()
        .map(|m| {
            Ok((
                m["source_path"]
                    .as_str()
                    .map(PathBuf::from)
                    .context("batch member has no source path")?,
                id(m, "album")?,
                id(m, "artist")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        members.len() == batch.targets.len(),
        "batch input and journal targets differ"
    );
    Ok(members)
}

/// What settling an earlier run's unfinished batch did.
pub(crate) struct Resumed {
    pub(crate) state: InsertBatchState,
    pub(crate) evidence: PathBuf,
    /// Tracks bound by the batch.
    pub(crate) inserted: usize,
    /// Tracks a session's removal half removed, observed now.
    pub(crate) removed: usize,
    /// The removal batch that took out the rows a Partial batch left
    /// without audio.
    pub(crate) compensation: Option<removal_batch::Settled>,
}

/// Remove the conversions of a batch an earlier run prepared and never sent,
/// now cancelled, with what that run converted ahead once it has ended.
pub(crate) fn remove_unsent_conversions(batch: &InsertBatchRecord) -> Result<()> {
    for ((path, ..), target) in retained_members(batch)?.iter().zip(&batch.targets) {
        if target.target.conversion.is_some() {
            flac_to_alac::remove_left(path)?;
        }
    }
    Ok(())
}

/// The journal names the collections the batch created; every target must be
/// in one of them or in a collection that existed before.
fn planned_groups(
    batch: &InsertBatchRecord,
    expected: &[ExpectedInsert<'_>],
    before: &music_protocol::catalogue::sqlite::Catalog,
) -> Result<()> {
    let created: BTreeSet<u64> = batch
        .targets
        .iter()
        .flat_map(|t| [t.target.new_album_pid, t.target.new_artist_pid])
        .flatten()
        .collect();
    for e in expected {
        for id in [e.album_id, e.artist_id] {
            ensure!(
                created.contains(&id) || before.represented_sync_ids().contains(&id),
                "track {:016x} joined an unplanned collection {id:016x}",
                e.track_id
            );
        }
    }
    Ok(())
}

/// Every prior indexed file but a session's `removing` is as before (its
/// allocation aside, `TrackAudio::same_file`), and only targets appeared. A
/// removed track's file is as before or no longer indexed.
fn prior_unchanged(
    before: &Snapshot,
    after: &Snapshot,
    targets: &BTreeSet<u64>,
    removing: &BTreeSet<u64>,
) -> Result<()> {
    for (id, audio) in before.index.tracks() {
        let now = after.index.tracks().get(id);
        ensure!(
            now.is_some_and(|a| a.same_file(audio)) || now.is_none() && removing.contains(id),
            "prior track {id:016x} changed during the insertion"
        );
    }
    for (id, audio) in before.index.other_audio() {
        let now = after.index.other_audio().get(id);
        ensure!(
            now.is_some_and(|a| a.same_file(audio)) || now.is_none() && removing.contains(id),
            "prior track {id:016x} changed during the insertion"
        );
    }
    for id in after
        .index
        .tracks()
        .keys()
        .chain(after.index.other_audio().keys())
    {
        ensure!(
            targets.contains(id)
                || before.index.tracks().contains_key(id)
                || before.index.other_audio().contains_key(id),
            "track {id:016x} appeared during the insertion"
        );
    }
    Ok(())
}
