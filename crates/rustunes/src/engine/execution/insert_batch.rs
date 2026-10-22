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

/// Prepare, send and settle one batch inserting `members` (in this order) into
/// the exact `before` state. With `removals`, the batch is a session that
/// removes those tracks first, in the same operation, as Apple's host sends a
/// sync's removals and additions; it is journaled as the insert batch and a
/// removal batch paired with it. Errors before Started leave nothing behind
/// but a Prepared batch (or session) that is cancelled here; after Started it
/// is settled by observation or left Started for `settle`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    journal: &mut Journal,
    identity: &Identity,
    before: &Before,
    removals: &[u64],
    members: &[Member],
    collection: [u8; 32],
    coordination: Option<CoordinationId>,
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    control.check()?;
    ensure!(!members.is_empty(), "insert batch has no members");
    ensure!(
        journal.outstanding_insert_batch()?.is_none(),
        "an earlier insert batch is unfinished; run resume first"
    );
    let removal_targets = match removals {
        [] => Vec::new(),
        _ => {
            ensure!(
                coordination.is_none(),
                "a session that removes tracks is no coordination's step"
            );
            ensure!(
                journal.outstanding_removal_batch()?.is_none(),
                "an earlier removal batch is unfinished; run resume first"
            );
            let items = removals
                .iter()
                .map(|t| before.item_pid(*t))
                .collect::<Result<Vec<_>>>()?;
            removal_sql::qualify_session(&before.sql.tables, &items)?;
            removal_batch::targets(before, journal, removals)?
        }
    };
    // Hold every source before planning: the operation binds these bytes.
    let mut held = hold(members)?;
    let imported_at =
        UNIX_EPOCH + Duration::from_secs(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    let (insertion, targets) = plan(before, journal, members, collection, imported_at)?;
    let current = &before.current;
    let operation = match removals {
        [] => insertion.clone(),
        _ => music_protocol::protocol::batch::with_removals(
            &mutation::delete_many(&current.catalog, removals, imported_at)?,
            &insertion,
        )?,
    };
    let base_anchor = before.anchor();
    let evidence = &before.directory;
    let mut input = json!({
        "schema":"rustunes.insert-batch-input",
        "base_anchor":base_anchor,
        "operation_sha256":hex(&sha(&operation)),
        "before_index_sha256":hex(&before.snapshot.index_receipt_sha256),
        "before_current_sha256":hex(&current.receipt_sha256),
        "collection_scope_sha256":hex(&collection),
        "host_name":host_name,
        "members":members.iter().zip(&targets).map(|(m, t)| json!({
            "source":m.relative_path,"source_path":m.source,
            "full_source_sha256":hex(&t.content.source.full_file_sha256),
            "file_bytes":t.content.source.file_bytes,
            "codec":t.content.source.format().codec_label(),
            "track":format!("{:016x}",m.track),"album":format!("{:016x}",m.album),
            "artist":format!("{:016x}",m.artist),"path":m.destination,
            "new_album":t.new_album_pid.map(|id| format!("{id:016x}")),
            "new_artist":t.new_artist_pid.map(|id| format!("{id:016x}")),
            "cover":t.content.artwork.as_ref().map(|a| json!({
                "cache_id":a.cache_id,"bytes":a.bytes,"sha256":hex(&a.sha256),
            })),
        })).collect::<Vec<_>>(),
    });
    if !removals.is_empty() {
        input["schema"] = json!("rustunes.session-input");
        input["removals"] = json!(removal_targets.iter().map(|t| json!({
            "track":format!("{:016x}",t.track),"item_pid":t.item_pid,"path":t.path,
            "file_bytes":t.file_bytes,"file_sha256":t.file_sha256.map(|h| hex(&h)),
            "binding":t.binding.map(|(id,version)| json!({"id":id.as_i64(),"version":version})),
        })).collect::<Vec<_>>());
    }
    let input = serde_json::to_vec_pretty(&input)?;
    durable_write(evidence, "input.json", &input)?;
    durable_write(evidence, "operation.plist", &operation)?;
    let input_sha = sha(&input);

    // A session also holds the ArtworkDB its removals leave as it is.
    let checked = check_session(
        before,
        members,
        &operation,
        input_sha,
        host_name,
        !removals.is_empty(),
        |state| {
            crate::engine::execution::native_insert::validate_batch(
                state,
                &insertion,
                targets
                    .iter()
                    .map(|t| (&t.content, [t.new_album_pid, t.new_artist_pid])),
            )
        },
    )?;
    let intent = InsertIntent {
        coordination,
        operation: operation.clone(),
        base_anchor,
        before_current: current.receipt_sha256,
        before_index: before.snapshot.index_receipt_sha256,
        input_receipt: input_sha,
        evidence: evidence.clone(),
        targets,
    };
    let (id, session) = match removals {
        [] => (journal.prepare_insert_batch(&intent)?, None),
        _ => {
            let ids = journal.prepare_session(
                &intent,
                &RemovalIntent {
                    coordination: None,
                    compensates: None,
                    compensates_insert_batch: None,
                    operation: operation.clone(),
                    base_anchor,
                    before_current: current.receipt_sha256,
                    before_index: before.snapshot.index_receipt_sha256,
                    input_receipt: input_sha,
                    evidence: evidence.clone(),
                    targets: removal_targets,
                    playlists: Vec::new(),
                },
            )?;
            (ids.insert, Some(ids))
        }
    };
    let send = evidence.join("send");
    let wire = match send_session(
        before,
        identity,
        members,
        &mut held,
        checked,
        &operation,
        evidence,
        control,
        // From here the batch is Started: nothing on the phone changed yet,
        // and only observation may settle it.
        |ready, signature| match session {
            Some(ids) => journal.start_session(ids, ready, signature),
            None => journal.start_insert_batch(id, ready, signature),
        },
    )
    .await
    {
        Sent::Unsent(error) => {
            match session {
                Some(ids) => journal.cancel_session(ids)?,
                None => journal.cancel_insert_batch(id)?,
            }
            let _ = durable_write(
                &send,
                "error.txt",
                format!("{error:#}\nNothing was written to the phone; the batch was cancelled.\n")
                    .as_bytes(),
            );
            return Err(error);
        }
        Sent::Started(wire) => wire.and_then(|wire| {
            durable_write(
                &send,
                "wire.json",
                &serde_json::to_vec_pretty(&json!({
                    "schema":"rustunes.insert-batch-wire","batch":id.as_i64(),
                    "ready_sha256":hex(&wire.ready),"terminal_sha256":hex(&wire.terminal),
                    "operation_sha256":hex(&wire.operation),"uploaded":wire.uploaded,
                }))?,
            )?;
            sync_trace(&send)
        }),
    };
    if let Err(error) = &wire {
        let _ = durable_write(
            &send,
            "error.txt",
            format!(
                "{error:#}\nBatch {} is Started; it is observed below.\n",
                id.as_i64()
            )
            .as_bytes(),
        );
    }
    let batch = journal.insert_batch(id)?;
    let planned = members
        .iter()
        .map(|m| {
            Ok(Planned {
                source: m.audio.expected()?,
                album: m.album,
                artist: m.artist,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    match (
        settle_with(
            journal,
            identity,
            &batch,
            &planned,
            Window::SameRun,
            control,
        )
        .await,
        wire,
    ) {
        (Ok(settled), _) => Ok(settled),
        (Err(error), Ok(())) => Err(error.context(format!(
            "insert batch {} is Started; run resume to observe it again",
            id.as_i64()
        ))),
        (Err(error), Err(wire)) => Err(error.context(format!(
            "insert batch {} is Started after a transport error ({wire:#}); run resume to observe it again",
            id.as_i64()
        ))),
    }
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

/// Bind the exact `before` state, check `operation` against it with
/// `validate`, and size the session that sends it with `members`' uploads.
/// The ArtworkDB is held unchanged through Ready when a member has a cover,
/// or with `hold_artwork`.
pub(crate) fn check_session(
    before: &Before,
    members: &[Member],
    operation: &[u8],
    input_receipt: [u8; 32],
    host_name: &str,
    hold_artwork: bool,
    validate: impl FnOnce(&FreshState) -> Result<()>,
) -> Result<Checked> {
    let current = &before.current;
    let base_anchor = before.anchor();
    let mut state = FreshState::new(
        &current.device,
        &current.catalog,
        &current.raw_preferences,
        &current.plist_preferences,
        input_receipt,
    )?;
    state.bind_copied_cdb(&current.cdb)?;
    ensure!(
        state.media_anchor() == base_anchor,
        "insert state anchor differs from the snapshot"
    );
    validate(&state)?;
    let library_id = current.library.library_id().to_owned();
    audio_policy(&state, members)?.ensure_session(&library_id, base_anchor)?;
    let covered = members.iter().filter(|m| m.cover.is_some()).count();
    let artwork_db = match covered == 0 && !hold_artwork {
        true => None,
        false => {
            let saved = current
                .artwork_db
                .as_deref()
                .context("artwork database capture absent")?;
            Some((saved.len() as u64, sha(saved)))
        }
    };
    let templates = templates::build_from_copied_cdb(
        &current.cdb,
        &current.catalog,
        &current.library,
        host_name,
    )?;
    let session = session::prepare(
        templates.request,
        &templates.finished,
        &library_id,
        base_anchor,
    )?;
    // Covers add, per guard, the saved ArtworkDB readback and each stage's
    // absence; per upload, the cover's write and readback.
    let guard_operations = crate::engine::execution::native_insert::batch_guard_budget(
        &state,
        members.len(),
        operation.len(),
    )? + artwork_db.map_or(0, |(bytes, _)| {
        2 * (bytes.div_ceil(CHUNK) as usize + 4 * covered + 8)
    });
    ensure!(
        guard_operations <= MAX_SESSION_OPS,
        "insert batch guard allowance exceeds bound"
    );
    // Uploads write 1 MiB per request; each file's allowance keeps the
    // single-insert margin (two per request, plus fixed checks).
    let upload_operations = members
        .iter()
        .map(|m| {
            let requests = |bytes: u64| {
                bytes.div_ceil(music_protocol::protocol::afc::copy::CHUNK as u64) as usize * 2
            };
            let cover = m
                .cover
                .as_ref()
                .map_or(0, |c| requests(c.bytes.len() as u64) + 16);
            let bytes = m.audio.transferred().file_bytes;
            crate::engine::execution::native_insert::upload_budget(bytes)?;
            Ok(requests(bytes) + 64 + cover)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Checked {
        state,
        session,
        guard_operations,
        upload_connections: connections(&upload_operations),
        artwork_db,
    })
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

/// Insert `members` in order, at most `batch_size` per session. Each batch is
/// planned from a fresh capture under `run/batch-NNN` and settled before the
/// next starts. The first batch's session removes `removals` (fewer than
/// `batch_size`, what a sync's removal left it) before its insertions, in
/// the same operation budget; when no batch runs they go in removal batches
/// of their own under `run/removal`. Returns the batches and the tracks
/// removed. `progress(settled, inserted, total)` runs after each batch, and
/// `left_out(member)` for each cover a batch leaves out to keep its album's
/// image. A batch that is not Verified stops the run; one that was undone
/// cleanly stops it with `CutShort`. FLACs to convert need `converter`, which
/// converts a few at a time ahead of their batches (`lookahead`), and
/// `refused(member, why)` runs for each that is not sent, as its batch
/// begins.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn add_all(
    journal_path: &Path,
    identity: &Identity,
    mut removals: Vec<u64>,
    members: &mut [Member],
    collection: [u8; 32],
    batch_size: usize,
    coordination: Option<CoordinationId>,
    run_directory: &Path,
    host_name: &str,
    control: impl Fn() -> Control,
    converter: Option<&flac_to_alac::Converter>,
    mut progress: impl FnMut(&Settled, usize, usize) -> Result<()>,
    mut left_out: impl FnMut(&Member) -> Result<()>,
    mut refused: impl FnMut(&Member, &anyhow::Error) -> Result<()>,
) -> Result<(Vec<Settled>, usize)> {
    ensure!(
        removals.len() < batch_size,
        "a batch's session removes fewer tracks than the batch size"
    );
    let total = members.len();
    let bytes = members
        .iter()
        .map(|m| std::fs::metadata(&m.source).map_or(0, |m| m.len()))
        .sum();
    let mut conversions = match converter {
        Some(converter) => {
            lookahead::Lookahead::ffmpeg(members, batch_size, converter, control().cancelled)?
        }
        None => {
            ensure!(
                members.iter().all(|m| !m.convert),
                "sending a FLAC as ALAC needs FFmpeg (--flac-to-alac)"
            );
            None
        }
    };
    let mut settled = Vec::new();
    let mut inserted = 0;
    let mut removed = 0;
    let mut start = 0;
    for number in 0.. {
        if start == total {
            break;
        }
        // The removals take their share of the first session's budget. A
        // batch of FLACs ends where their conversions stop fitting.
        let budget = batch_size - removals.len();
        let end = match conversions.as_ref() {
            Some(conversions) => conversions.fitting(start, total.min(start + budget))?,
            None => total.min(start + budget),
        };
        let offset = std::mem::replace(&mut start, end);
        let batches = number + 1 + (total - end).div_ceil(batch_size);
        let chunk = &mut members[offset..end];
        let sendable = match conversions.as_mut() {
            Some(conversions) => conversions.batch(chunk, offset, &mut refused).await?,
            None => chunk.len(),
        };
        let chunk = &mut chunk[..sendable];
        if chunk.is_empty() {
            continue;
        }
        crate::engine::observe(crate::engine::Event::InsertBatchStarted {
            number: number + 1,
            batches,
            tracks: chunk.len(),
            inserted,
            total,
            bytes,
            removals: removals.len(),
        })?;
        let control = control();
        let directory = run_directory.join(format!("batch-{:03}", number + 1));
        // A removal target's file may be gone after an earlier removal of it
        // that the phone rolled back.
        let before = match removals.is_empty() {
            true => removal_batch::capture_before(&directory, identity, &control).await?,
            false => {
                let taken = removal_batch::taken(journal_path, identity)?;
                removal_batch::capture_for_removal(&directory, identity, &taken, &control).await?
            }
        };
        for member in keep_album_images(
            &before.current,
            &before.directory,
            chunk,
            &control,
            device::provider,
        )
        .await?
        {
            left_out(member)?;
        }
        let chunk = &*chunk;
        let mut journal = Journal::open(journal_path, identity)?;
        let session = std::mem::take(&mut removals);
        let batch = run(
            &mut journal,
            identity,
            &before,
            &session,
            chunk,
            collection,
            coordination,
            host_name,
            &control,
        )
        .await
        .with_context(|| {
            format!(
                "{inserted} of {} tracks were inserted before this batch",
                total
            )
        })?;
        drop(journal);
        removed += batch.removed;
        // The batch is settled: its conversions are done with.
        for member in chunk.iter().filter(|m| m.conversion.is_some()) {
            flac_to_alac::remove(&member.source)?;
        }
        if batch.state == InsertBatchState::Partial {
            let record = Journal::open(journal_path, identity)?.insert_batch(batch.batch)?;
            let unfilled = unfilled(&record).len();
            let removal = compensate(journal_path, identity, &record, host_name, &control)
                .await
                .with_context(|| {
                    format!(
                        "insert batch {} left {unfilled} rows without audio and their removal did not finish; run resume",
                        batch.batch.as_i64()
                    )
                })?;
            ensure!(
                removal.state == crate::engine::state::journal::BatchState::Verified,
                "insert batch {} left {unfilled} rows without audio; removal batch {} ended {}; run resume",
                batch.batch.as_i64(),
                removal.batch.as_i64(),
                removal.state.as_str()
            );
            return Err(CutShort(
                format!(
                    "insert batch {} added {} of its {} tracks; removal batch {} took the {unfilled} rows without audio out again. {} of {} tracks were inserted",
                    batch.batch.as_i64(),
                    batch.inserted,
                    chunk.len(),
                    removal.batch.as_i64(),
                    inserted + batch.inserted,
                    total
                ),
                removed,
            )
            .into());
        }
        if batch.state == InsertBatchState::Compensated {
            return Err(CutShort(
                format!(
                    "insert batch {} added {} of its {} tracks; the phone took the others out again itself. {} of {} tracks were inserted",
                    batch.batch.as_i64(),
                    batch.inserted,
                    chunk.len(),
                    inserted + batch.inserted,
                    total
                ),
                removed,
            )
            .into());
        }
        ensure!(
            batch.state == InsertBatchState::Verified,
            "insert batch {} ended {}; {inserted} of {} tracks were inserted",
            batch.batch.as_i64(),
            batch.state.as_str(),
            total
        );
        inserted += batch.inserted;
        progress(&batch, inserted, total)?;
        settled.push(batch);
    }
    if !removals.is_empty() {
        let directory = run_directory.join("removal");
        create_trace(&directory)?;
        let alone = Box::pin(removal_batch::remove_all(
            journal_path,
            identity,
            &removals,
            batch_size,
            coordination,
            &directory,
            host_name,
            control,
            |_, _, _| Ok(()),
        ))
        .await?;
        removed += alone.iter().map(|b| b.removed).sum::<usize>();
    }
    Ok((settled, removed))
}

/// Clear the cover of each member that must keep its album's image unless
/// the album's permanent original in `current` is exactly that cover, and
/// record each finding in `directory/album-images.json`. A kept cover leaves
/// the album's image as it was: the album keeps its registration or takes a
/// new member's cover, and every cover a member of that album carries holds
/// the same bytes. Each album and cover is read back once. Returns the
/// members left without their cover. A read that cannot finish stops the
/// run before the batch.
async fn keep_album_images<'a, F, Fut>(
    current: &current::Current,
    directory: &Path,
    chunk: &'a mut [Member],
    control: &Control,
    connect: F,
) -> Result<Vec<&'a Member>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<device::DeviceProvider>>,
{
    let mut proofs = BTreeMap::new();
    let mut record = Vec::new();
    let mut cleared = Vec::new();
    for (index, member) in chunk.iter_mut().enumerate() {
        if !member.keep_album_image {
            continue;
        }
        let Some(cover) = &member.cover else {
            continue;
        };
        let key = (member.album, cover.sha256);
        if !proofs.contains_key(&key) {
            // Each read has its own 120-second bound.
            let read_control = Control {
                deadline: control
                    .deadline
                    .min(Instant::now() + Duration::from_secs(120)),
                cancelled: Arc::clone(&control.cancelled),
            };
            let equal = crate::engine::verification::artwork::album_original_equal(
                current,
                member.album,
                &crate::engine::verification::artwork::OriginalArtworkExpectation {
                    bytes: cover.bytes.len() as u64,
                    sha256: cover.sha256,
                },
                &directory.join(format!("album-image-{:03}", proofs.len() + 1)),
                &read_control,
                &connect,
            )
            .await
            .with_context(|| {
                format!(
                    "the image of the album {} joins could not be read; nothing was sent in this batch",
                    member.relative_path
                )
            })?;
            proofs.insert(key, equal.map(|_| ()));
        }
        let equal = &proofs[&key];
        record.push(json!({
            "source":member.relative_path,"album":format!("{:016x}",member.album),
            "cover_sha256":hex(&cover.sha256),"cover_bytes":cover.bytes.len(),
            "album_image_equal":equal.is_ok(),"reason":equal.as_ref().err(),
        }));
        if equal.is_err() {
            member.cover = None;
            cleared.push(index);
        }
    }
    if !record.is_empty() {
        durable_write(
            directory,
            "album-images.json",
            &serde_json::to_vec_pretty(&record)?,
        )?;
    }
    let chunk: &'a [Member] = chunk;
    Ok(cleared.into_iter().map(|index| &chunk[index]).collect())
}

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

/// Settle an earlier run's unfinished batch, a session's with its removal
/// half: a Prepared one was never sent and is cancelled, its conversions
/// removed; a Started one is observed (`settle`), never resent; the rows a
/// Partial one left without audio are removed by one compensating removal
/// batch, whose host announces `host_name`.
pub(crate) async fn resume(
    journal_path: &Path,
    identity: &Identity,
    batch: &InsertBatchRecord,
    host_name: &str,
    control: &Control,
) -> Result<Resumed> {
    let mut journal = Journal::open(journal_path, identity)?;
    let id = batch.id.as_i64();
    let bound = |batch: &InsertBatchRecord| {
        batch
            .targets
            .iter()
            .filter(|t| t.outcome == Some(TargetOutcome::Bound))
            .count()
    };
    let mut resumed = match batch.state {
        InsertBatchState::Prepared => {
            match journal.session_removal(batch.id)? {
                Some(removal) => journal.cancel_session(SessionIds {
                    insert: batch.id,
                    removal: removal.id,
                })?,
                None => journal.cancel_insert_batch(batch.id)?,
            }
            remove_unsent_conversions(batch)?;
            Resumed {
                state: InsertBatchState::Cancelled,
                evidence: batch.evidence.clone(),
                inserted: 0,
                removed: 0,
                compensation: None,
            }
        }
        InsertBatchState::Started => {
            let settled = settle(&mut journal, identity, batch, control).await?;
            Resumed {
                state: settled.state,
                evidence: settled.evidence,
                inserted: settled.inserted,
                removed: settled.removed,
                compensation: None,
            }
        }
        InsertBatchState::Partial => Resumed {
            state: InsertBatchState::Partial,
            evidence: batch.evidence.clone(),
            inserted: bound(batch),
            removed: 0,
            compensation: None,
        },
        other => bail!("insert batch {id} is {other:?} and needs no recovery"),
    };
    if resumed.state == InsertBatchState::Partial {
        let record = journal.insert_batch(batch.id)?;
        drop(journal);
        resumed.compensation =
            Some(compensate(journal_path, identity, &record, host_name, control).await?);
        resumed.state = Journal::open(journal_path, identity)?
            .insert_batch(batch.id)?
            .state;
    }
    Ok(resumed)
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

/// Observe a Started batch again (after an interrupted run). Each source must
/// still be the exact file the batch sent: its metadata is the expected row.
/// A conversion that is gone is made again from its FLAC and must be the
/// same file; once the batch settles its conversions are removed.
pub(crate) async fn settle(
    journal: &mut Journal,
    identity: &Identity,
    batch: &InsertBatchRecord,
    control: &Control,
) -> Result<Settled> {
    let members = retained_members(batch)?;
    let mut sources = Vec::with_capacity(members.len());
    let mut paths = Vec::with_capacity(members.len());
    let mut gone = Vec::new();
    let mut reconverted = None;
    for ((path, ..), target) in members.iter().zip(&batch.targets) {
        let sent = &target.target.content.source;
        let path = match &target.target.conversion {
            // A reboot empties the temporary directory.
            Some(conversion) if !path.try_exists()? => {
                gone.push(path.clone());
                let directory = match &mut reconverted {
                    Some(directory) => directory,
                    None => reconverted.insert(flac_to_alac::Directory::create()?),
                };
                flac_to_alac::reconvert(conversion, sent, directory).with_context(|| {
                    format!(
                        "the conversion {} of batch {} is gone and cannot be made again",
                        path.display(),
                        batch.id.as_i64()
                    )
                })?
            }
            _ => path.clone(),
        };
        sources.push(Audio::reread(&path, sent).with_context(|| {
            format!(
                "source {} of batch {} cannot be observed; restore it to observe the batch",
                path.display(),
                batch.id.as_i64()
            )
        })?);
        paths.push(path);
    }
    let planned = sources
        .iter()
        .zip(&members)
        .map(|(source, (_, album, artist))| {
            Ok(Planned {
                source: source.expected()?,
                album: *album,
                artist: *artist,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // A later run: the phone may have been played since the session.
    let settled = settle_with(
        journal,
        identity,
        batch,
        &planned,
        Window::LaterRun,
        control,
    )
    .await?;
    // With the directories that held conversions now gone, once empty.
    for (path, target) in paths.iter().zip(&batch.targets) {
        if target.target.conversion.is_some() {
            flac_to_alac::remove_left(path)?;
        }
    }
    for path in &gone {
        flac_to_alac::remove_left(path)?;
    }
    if let Some(directory) = reconverted {
        directory.finish()?;
    }
    Ok(settled)
}

/// Verified when every target row and permanent file is exact; Partial when
/// some audio never arrived (those rows have no file location); Compensated
/// when every target without its audio was deleted again by the phone;
/// NotApplied when nothing moved and nothing is staged but this batch's own
/// unfinished operation pair, which is removed first. Otherwise the batch
/// stays Started and the error says why.
async fn settle_with(
    journal: &mut Journal,
    identity: &Identity,
    batch: &InsertBatchRecord,
    planned: &[Planned<'_>],
    window: Window,
    control: &Control,
) -> Result<Settled> {
    ensure!(
        batch.state == InsertBatchState::Started,
        "insert batch {} is not Started",
        batch.id.as_i64()
    );
    // A session's removal half: its targets leave in the same observation in
    // which the insert half's arrive.
    let removal = journal.session_removal(batch.id)?;
    if let Some(removal) = &removal {
        ensure!(
            removal.state == BatchState::Started,
            "removal batch {} of the session is not Started",
            removal.id.as_i64()
        );
    }
    let removing: BTreeSet<u64> = removal
        .iter()
        .flat_map(|r| r.targets.iter().map(|t| t.track))
        .collect();
    let evidence = &batch.evidence;
    // A removal target's file may be gone: the phone deletes it with the
    // session, or did with an earlier removal it rolled back.
    let before = snapshot::open_allowing(
        &evidence.join("snapshot"),
        Absent {
            gone: removal.is_some(),
            ..Absent::NONE
        },
    )?;
    ensure!(
        before.index_receipt_sha256 == batch.before_index,
        "retained before snapshot differs from the journaled batch"
    );
    let run = next_directory(evidence, "observe")?;
    create_trace(&run)?;
    let after_dir = run.join("snapshot");
    // Only the batch's own rows may lack audio: an interrupted session leaves
    // them so, and they are what compensation removes.
    let tracks: BTreeSet<u64> = batch
        .targets
        .iter()
        .map(|t| t.target.content.track_pid)
        .collect();
    let absent = Absent {
        unfilled: &tracks,
        gone: removal.is_some(),
        ..Absent::NONE
    };
    crate::engine::verification::audio_capture::capture_allowing(
        &after_dir,
        absent,
        control,
        device::provider,
    )
    .await?;
    let after = snapshot::open_allowing(&after_dir, absent)?;
    ensure!(
        before.index.gone().is_subset(&removing) && after.index.gone().is_subset(&removing),
        "an audio file missing from the phone belongs to no removal of batch {}; it stays Started",
        batch.id.as_i64()
    );
    prior_unchanged(&before, &after, &tracks, &removing)?;
    let anchor = after.catalog.persisted_media_anchor();
    let present: Vec<_> = batch
        .targets
        .iter()
        .map(|t| {
            after
                .catalog
                .tracks()
                .iter()
                .find(|row| row.sync_id == t.target.content.track_pid)
        })
        .collect();
    if anchor == batch.base_anchor {
        ensure!(
            present.iter().all(Option::is_none)
                && after.catalog.tracks().len() == before.catalog.tracks().len(),
            "the anchor did not move but the catalogue changed; batch {} stays Started",
            batch.id.as_i64()
        );
        if let Some(removal) = &removal {
            removals_unapplied(&before, &after, removal, &run, window, control)?;
        }
        let (staged, airlock) = staged_names(identity, &after, &run, batch, control).await?;
        let staged_removed = removal_batch::remove_unfinished(
            identity,
            &music_protocol::catalogue::identity::parse(&after.raw_preferences)?,
            &run,
            &staged,
            stale_staging::Unfinished {
                send: &batch.evidence.join("send"),
                operation: &batch.operation,
                signature: batch.signature,
            },
            batch.id.as_i64(),
            control,
        )
        .await?;
        let mut proof = json!({
            "schema":"rustunes.insert-batch-not-applied","batch":batch.id.as_i64(),
            "anchor":anchor,"after_index_sha256":hex(&after.index_receipt_sha256),
            "targets_absent":true,"staging_empty":true,"staged_removed":staged_removed,
            "uploaded_unclaimed":airlock,
        });
        if let Some(removal) = &removal {
            proof["session_removal"] = json!({
                "batch":removal.id.as_i64(),
                "targets_present":removal.targets.iter().map(|t| t.item_pid).collect::<Vec<_>>(),
                "target_files_gone":after.index.gone().iter().map(|t| format!("{t:016x}")).collect::<Vec<_>>(),
            });
        }
        let proof = serde_json::to_vec_pretty(&proof)?;
        durable_write(&run, "not-applied.json", &proof)?;
        match &removal {
            Some(removal) => journal.mark_session_not_applied(
                SessionIds {
                    insert: batch.id,
                    removal: removal.id,
                },
                sha(&proof),
            )?,
            None => journal.mark_insert_batch_not_applied(batch.id, sha(&proof))?,
        }
        return Ok(Settled {
            batch: batch.id,
            state: InsertBatchState::NotApplied,
            evidence: run,
            inserted: 0,
            removed: 0,
        });
    }
    // The session's removals went with its operation, every one.
    if let Some(track) = removing
        .iter()
        .find(|t| after.catalog.tracks().iter().any(|row| row.sync_id == **t))
    {
        bail!(
            "removal target {track:016x} is still on the phone though the anchor moved; batch {} stays Started",
            batch.id.as_i64()
        );
    }
    // A target is bound when its row holds the planned path and the complete
    // index read exactly the sent bytes there; a row the phone left without a
    // file location awaits compensation; a target without a row was deleted
    // again by the phone, which the catalogue check and the absence of its
    // planned file confirm. Anything else is unexplained.
    let mut bound = Vec::new();
    let mut locationless = BTreeSet::new();
    let mut removed = BTreeMap::new();
    for (ordinal, (target, row)) in batch.targets.iter().zip(&present).enumerate() {
        let content = &target.target.content;
        let Some(row) = row else {
            removed.insert(ordinal, content.track_pid);
            continue;
        };
        match &row.afc_path {
            None => {
                ensure!(
                    after.index.unfilled().contains(&content.track_pid),
                    "track {:016x} has no file location but is not an unfilled row; batch {} stays Started",
                    content.track_pid,
                    batch.id.as_i64()
                );
                locationless.insert(content.track_pid);
            }
            Some(path) => {
                // The index reads an ALAC file as a track's audio and an MP3
                // as other audio.
                let indexed = if content.source.is_alac() {
                    after.index.tracks().get(&content.track_pid).map(|a| {
                        (
                            &a.afc_path,
                            a.fingerprint.file_bytes,
                            a.fingerprint.full_file_sha256,
                        )
                    })
                } else {
                    after
                        .index
                        .other_audio()
                        .get(&content.track_pid)
                        .map(|a| (&a.afc_path, a.file_bytes, a.full_file_sha256))
                };
                let (afc_path, file_bytes, sha256) = indexed.with_context(|| {
                    format!(
                        "track {:016x} has a path but no index entry",
                        content.track_pid
                    )
                })?;
                ensure!(
                    path == &content.path
                        && afc_path == &content.path
                        && file_bytes == content.source.file_bytes
                        && sha256 == content.source.full_file_sha256,
                    "track {:016x} does not hold the sent audio at its planned path; batch {} stays Started",
                    content.track_pid,
                    batch.id.as_i64()
                );
                bound.push((ordinal, sha256));
            }
        }
    }
    let expected: Vec<_> = batch
        .targets
        .iter()
        .zip(planned)
        .map(|(t, p)| {
            let content = &t.target.content;
            ExpectedInsert {
                track_id: content.track_pid,
                album_id: p.album,
                artist_id: p.artist,
                path: &content.path,
                file_bytes: content.source.file_bytes,
                metadata: p.source,
                presented: true,
            }
        })
        .collect();
    planned_groups(batch, &expected, &before.catalog)?;
    let covers: BTreeMap<u64, u32> = batch
        .targets
        .iter()
        .filter_map(|t| {
            let content = &t.target.content;
            content
                .artwork
                .as_ref()
                .map(|a| (content.track_pid, a.cache_id))
        })
        .collect();
    let removed_tracks: BTreeSet<u64> = removed.values().copied().collect();
    // A session's insertions are checked against the catalogue its removals
    // leave.
    let projected = match &removal {
        Some(_) => Some(before.catalog.without_tracks(&removing, &after.catalog)?),
        None => None,
    };
    let prior = projected.as_ref().unwrap_or(&before.catalog);
    let catalogue = match window {
        Window::SameRun => insert_verification::verify_insert_batch_covered(
            prior,
            &after.catalog,
            &expected,
            &locationless,
            &removed_tracks,
            &covers,
        ),
        Window::LaterRun => insert_verification::verify_insert_batch_after_listening(
            prior,
            &after.catalog,
            &expected,
            &locationless,
            &removed_tracks,
            &covers,
            &between_runs::catalogue_listening,
        ),
    }
    .with_context(|| {
                format!(
                    "the phone's catalogue is not the planned insertion; batch {} stays Started and {} keeps the after state",
                    batch.id.as_i64(),
                    run.display()
                )
            })?;
    let removed_paths: Vec<&str> = removed
        .keys()
        .map(|ordinal| batch.targets[*ordinal].target.content.path.as_str())
        .collect();
    if !removed_paths.is_empty() {
        planned_absent(identity, &after, &run, &removed_paths, control)
            .await
            .with_context(|| {
                format!(
                    "batch {} stays Started and {} keeps the after state",
                    batch.id.as_i64(),
                    run.display()
                )
            })?;
    }
    if let Some(removal) = &removal {
        let library = music_protocol::catalogue::identity::parse(&before.raw_preferences)?;
        let remaining =
            removal_batch::wait_absent(identity, &library, &run, removal, control).await?;
        ensure!(
            remaining.is_empty(),
            "removed tracks' files are still on the phone: {}; batch {} stays Started",
            remaining.join(", "),
            batch.id.as_i64()
        );
    }
    let checks: Vec<TrackCheck> = bound
        .iter()
        .filter_map(|(ordinal, _)| {
            let content = &batch.targets[*ordinal].target.content;
            content.artwork.as_ref().map(|a| TrackCheck {
                target: content.track_pid,
                expected: TrackArtworkExpectation {
                    cache_id: a.cache_id,
                    bytes: a.bytes,
                    sha256: a.sha256,
                },
                trace: run.join(format!("artwork-{ordinal:04}")),
            })
        })
        .collect();
    let permanent_covers = if checks.is_empty() {
        Vec::new()
    } else {
        verify_covers(&after, &run, identity, &checks, &locationless, control)
            .await
            .with_context(|| {
                format!(
                    "the permanent covers are not verified; batch {} stays Started and {} keeps the after state",
                    batch.id.as_i64(),
                    run.display()
                )
            })?
    };
    let mut receipt = json!({
        "schema":"rustunes.insert-batch-verification","batch":batch.id.as_i64(),
        "base_anchor":batch.base_anchor,"verified_anchor":anchor,
        "before_index_sha256":hex(&batch.before_index),
        "after_index_sha256":hex(&after.index_receipt_sha256),
        "catalogue":catalogue,"prior_files_unchanged":true,
        "bound":bound.iter().map(|(n, h)| json!({"ordinal":n,"audio_sha256":hex(h)})).collect::<Vec<_>>(),
        "locationless":locationless.iter().map(|id| format!("{id:016x}")).collect::<Vec<_>>(),
    });
    if !removed.is_empty() {
        receipt["removed_by_phone"] = json!(
            removed
                .iter()
                .map(|(n, id)| json!({
                    "ordinal":n,"track":format!("{id:016x}"),
                    "planned_path_absent":batch.targets[*n].target.content.path,
                }))
                .collect::<Vec<_>>()
        );
    }
    if !permanent_covers.is_empty() {
        receipt["permanent_covers"] = serde_json::to_value(&permanent_covers)?;
    }
    if let Some(removal) = &removal {
        receipt["session_removal"] = json!({
            "batch":removal.id.as_i64(),
            "removed":removal.targets.iter().map(|t| format!("{:016x}", t.track)).collect::<Vec<_>>(),
            "catalogue_checked_without_them":true,"target_files_absent":true,
        });
    }
    removal_batch::allocation_changed(&mut receipt, &before, &after);
    let receipt = serde_json::to_vec_pretty(&receipt)?;
    durable_write(&run, "verification.json", &receipt)?;
    let receipt_sha = sha(&receipt);
    let evidence: Vec<_> = bound
        .iter()
        .map(|(ordinal, audio)| {
            let content = &batch.targets[*ordinal].target.content;
            (
                *ordinal,
                CompletionEvidence {
                    track_pid: content.track_pid,
                    path: content.path.clone(),
                    audio_bytes: content.source.file_bytes,
                    audio_file_sha256: *audio,
                    artwork_sha256: content.artwork.as_ref().map(|a| a.sha256),
                    media_anchor: anchor,
                    receipt_sha256: receipt_sha,
                },
            )
        })
        .collect();
    let removed: Vec<usize> = removed.into_keys().collect();
    // One receipt covers both halves of a session.
    let state = match &removal {
        Some(removal) => journal.settle_session(
            SessionIds {
                insert: batch.id,
                removal: removal.id,
            },
            receipt_sha,
            receipt_sha,
            anchor,
            &evidence,
            &removed,
        )?,
        None => journal.settle_insert_batch(batch.id, receipt_sha, anchor, &evidence, &removed)?,
    };
    Ok(Settled {
        batch: batch.id,
        state,
        evidence: run,
        inserted: bound.len(),
        removed: removing.len(),
    })
}

/// Read back each bound target's permanent cover over a capture of the same
/// after state, as `verify` does: the exact original and a decodable
/// derivative. Proofs come back in `checks` order.
async fn verify_covers(
    after: &Snapshot,
    run: &Path,
    identity: &Identity,
    checks: &[TrackCheck],
    locationless: &BTreeSet<u64>,
    control: &Control,
) -> Result<Vec<crate::engine::verification::artwork::VerifiedTrackArtworkBytes>> {
    cover::require_decoder()?;
    let current = current::capture_with_artwork_controlled(
        &run.join("current"),
        identity,
        control.deadline,
        Arc::clone(&control.cancelled),
    )
    .await?;
    let (a, b) = (
        current.catalog.source_hashes(),
        after.catalog.source_hashes(),
    );
    ensure!(
        a.database == b.database
            && a.database_bytes == b.database_bytes
            && a.wal == b.wal
            && a.wal_bytes == b.wal_bytes,
        "the phone's catalogue changed after the batch was observed"
    );
    let mut proofs = Vec::with_capacity(checks.len());
    while proofs.len() < checks.len() {
        control.check()?;
        let done = proofs.len();
        // Each group's readback has its own 120-second bound.
        let group_control = Control {
            deadline: control
                .deadline
                .min(Instant::now() + Duration::from_secs(120)),
            cancelled: Arc::clone(&control.cancelled),
        };
        let group = crate::engine::verification::artwork::verify_track_group(
            &current,
            &checks[done..],
            &run.join(format!("artwork-group-{done:04}")),
            &group_control,
            locationless,
            device::provider,
            |_| Ok(()),
        )
        .await
        .map_err(|(position, error)| {
            error.context(format!(
                "permanent cover of track {:016x}",
                checks[done + position].target
            ))
        })?;
        proofs.extend(group);
    }
    Ok(proofs)
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

/// The removal half of a session the phone did not apply: the complete
/// catalogue is as before (`removal_sql::not_applied`, or `not_applied_later`
/// once the phone may have been played).
fn removals_unapplied(
    before: &Snapshot,
    after: &Snapshot,
    removal: &BatchRecord,
    run: &Path,
    window: Window,
    control: &Control,
) -> Result<()> {
    let before_sql = removal_batch::materialize(before, &run.join("sql-before"), control)?;
    let after_sql = removal_batch::materialize(after, &run.join("sql-after"), control)?;
    let items: Vec<i64> = removal.targets.iter().map(|t| t.item_pid).collect();
    let unchanged = match window {
        Window::SameRun => removal_sql::not_applied(&before_sql.tables, &after_sql.tables, &items)?,
        Window::LaterRun => {
            removal_sql::not_applied_later(&before_sql.tables, &after_sql.tables, &items)?
        }
    };
    ensure!(
        unchanged,
        "the anchor did not move but the catalogue changed; removal batch {} stays Started",
        removal.id.as_i64()
    );
    Ok(())
}

/// A bounded AFC connection to the journal's phone, traced under `run/name`.
async fn connect(
    identity: &Identity,
    after: &Snapshot,
    run: &Path,
    name: &str,
    operations: usize,
    control: &Control,
) -> Result<Transfer> {
    let library = music_protocol::catalogue::identity::parse(&after.raw_preferences)?;
    let trace = run.join(name);
    create_trace(&trace)?;
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(30));
    let provider = timeout_at(deadline, device::provider())
        .await
        .context("discovery deadline")??;
    ensure!(
        Identity::new(&provider.udid, &library)?.device_digest() == identity.device_digest(),
        "discovered device differs from the journal"
    );
    Transfer::connect_bounded(
        &provider,
        &trace,
        deadline,
        operations,
        Arc::clone(&control.cancelled),
    )
    .await
}

/// No file exists at the planned permanent path of any target the phone
/// deleted again.
async fn planned_absent(
    identity: &Identity,
    after: &Snapshot,
    run: &Path,
    paths: &[&str],
    control: &Control,
) -> Result<()> {
    let mut afc = connect(
        identity,
        after,
        run,
        "removed-paths",
        paths.len() * 4 + 16,
        control,
    )
    .await?;
    for path in paths {
        ensure!(
            afc.stat(path).await?.is_none(),
            "a file exists at {path}, where a target the phone deleted planned its audio"
        );
    }
    Ok(())
}

/// The metadata staging names (for NotApplied, at most this batch's own
/// unfinished pair, which is removed) and any of this batch's uploads left
/// unclaimed in the airlock.
async fn staged_names(
    identity: &Identity,
    after: &Snapshot,
    run: &Path,
    batch: &InsertBatchRecord,
    control: &Control,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut afc = connect(identity, after, run, "staging", 32, control).await?;
    let staged = afc
        .names(STAGING)
        .await?
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect();
    let ours: BTreeMap<String, ()> = batch
        .targets
        .iter()
        .flat_map(|t| {
            let id = (t.target.content.track_pid as i64).to_string();
            [
                (format!("{id}{}", suffix(&t.target.content.source)), ()),
                (id, ()),
            ]
        })
        .collect();
    let mut airlock: Vec<String> = afc
        .names_if_present(AIRLOCK)
        .await?
        .into_iter()
        .filter(|n| ours.contains_key(n))
        .collect();
    if batch
        .targets
        .iter()
        .any(|t| t.target.content.artwork.is_some())
    {
        airlock.extend(
            afc.names_if_present(ARTWORK)
                .await?
                .into_iter()
                .filter(|n| ours.contains_key(n))
                .map(|n| format!("Artwork/{n}")),
        );
    }
    Ok((staged, airlock))
}

/// Which uploads open a new connection, and its operation allowance: in
/// upload order (the operation's reverse), each connection takes files while
/// their allowances fit one connection's bound.
fn connections(operations: &[usize]) -> Vec<Option<usize>> {
    let mut opens = vec![None; operations.len()];
    let mut current: Option<(usize, usize)> = None;
    for index in (0..operations.len()).rev() {
        match &mut current {
            Some((_, total))
                if *total + operations[index]
                    <= music_protocol::protocol::transfer::MAX_BOUNDED_OPS =>
            {
                *total += operations[index];
            }
            _ => {
                if let Some((first, total)) = current {
                    opens[first] = Some(total);
                }
                current = Some((index, operations[index]));
            }
        }
    }
    if let Some((first, total)) = current {
        opens[first] = Some(total);
    }
    opens
}

mod lookahead;
#[cfg(test)]
mod tests;
