//! One signed native metadata-only session that removes several catalogue
//! rows, one `delete_track` per target, journaled as a removal batch.
//!
//! The batch is Prepared from a fresh complete snapshot and a fresh Current
//! that agree byte for byte, Started only after the live session is Ready and
//! the operation is signed, and settled only by observation: the complete
//! after-state must be the modelled removal (`removal_sql`) with every
//! surviving file unchanged and every target file gone. A session that
//! provably did not apply becomes NotApplied. Anything else stays Started for
//! `settle` to observe again; nothing is ever resent.
//!
//! The same batch compensates an unfinished insertion: it removes exactly the
//! rows the phone created without audio (no file location, no asset), with
//! every asset request for them refused (docs/05-receiver-operations.md,
//! section 1, "Interruption and recovery").
//!
//! A playlist batch deletes empty ordinary playlists instead of tracks, one
//! `delete_playlist` each (`run_playlists`; a Clear's last step once every
//! track is gone). It is settled the same way against its own model
//! (`removal_sql::verify_deletion`).
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, timeout_at};

use crate::{
    engine::execution::device,
    engine::execution::native_insert::{
        Control, FreshState, create_trace, durable_write, sync_trace,
    },
    engine::recovery::retained::playback::{Window, between_runs},
    engine::state::audio_index::Absent,
    engine::state::current::{self, Current},
    engine::state::journal::{
        AttemptId, BatchId, BatchRecord, BatchState, CoordinationId, Identity, InsertBatchId,
        Journal, PlaylistTarget, RemovalIntent, RemovalTarget, StartedBatch,
    },
    engine::state::snapshot::{self, Snapshot},
    engine::verification::audio_capture,
    engine::verification::sql_tables::{
        Capture,
        reader::{self, Budget, Cell},
    },
    engine::{
        recovery::stale_staging,
        verification::{removal_sql, sql_values as typed},
    },
};
use music_protocol::catalogue::identity::{self, LibraryIdentity};
use music_protocol::catalogue::mutation;
use music_protocol::protocol::session::{self, Session, StartedOperation};
use music_protocol::protocol::templates;
use music_protocol::protocol::transfer::{
    Attributes, MAX_ASSET, MAX_BOUNDED_OPS, METADATA, SIGNATURE, Transfer, regular_size,
};

const ARTWORK_DB: &str = "iTunes_Control/Artwork/ArtworkDB";
const STAGING: &str = "iTunes_Control/Sync/Media";
const STATE_FILES: [&str; 7] = [
    "iTunes_Control/iTunes/iTunesPrefs",
    "iTunes_Control/iTunes/iTunesPrefs.plist",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb-shm",
    "iTunes_Control/iTunes/iTunesCDB",
    ARTWORK_DB,
];
const CHUNK: u64 = 64 * 1024;
/// The phone removes deleted audio files shortly after the session ends.
const ASSET_WAIT: Duration = Duration::from_secs(120);
/// Observations of a Started batch before a run leaves it for the next run
/// (or resume): the phone may still be finishing the session, drop a
/// connection or remove files late.
const SETTLE_ATTEMPTS: usize = 3;
const SETTLE_PAUSE: Duration = Duration::from_secs(10);
/// Attempts per batch of a run: one the phone rolled back (NotApplied) or
/// that was never sent is sent once more, as a new batch from fresh state.
pub(crate) const BATCH_ATTEMPTS: usize = 2;

/// A complete before-state: snapshot (catalogue, complete audio index) and
/// Current (CDB, ArtworkDB) captured back to back with identical SQL bytes.
pub(crate) struct Before {
    pub(crate) directory: PathBuf,
    pub(crate) snapshot: Snapshot,
    pub(crate) current: Current,
    pub(crate) sql: reader::Snapshot,
}

impl Before {
    pub(crate) fn anchor(&self) -> u32 {
        self.current.catalog.persisted_media_anchor()
    }
    pub(crate) fn item_pid(&self, track: u64) -> Result<i64> {
        let store = self.sql.table("item_store")?;
        let row = typed::unique(store, "sync_id", Cell::Integer(track as i64))
            .with_context(|| format!("track {track:016x} is not in the catalogue"))?;
        typed::int(store, row, "item_pid")
    }
}

/// Capture a fresh complete snapshot and Current under `directory` (new).
pub(crate) async fn capture_before(
    directory: &Path,
    identity: &Identity,
    control: &Control,
) -> Result<Before> {
    capture(directory, Some(identity), Absent::NONE, control)
        .await
        .map(|(_, before)| before)
}

/// `capture_before` for a removal: a catalogued file may be missing only for
/// a track in `taken`, whose earlier removal may have deleted the file before
/// the phone rolled the session back.
pub(crate) async fn capture_for_removal(
    directory: &Path,
    identity: &Identity,
    taken: &BTreeSet<u64>,
    control: &Control,
) -> Result<Before> {
    let absent = Absent {
        gone: !taken.is_empty(),
        ..Absent::NONE
    };
    let (_, before) = capture(directory, Some(identity), absent, control).await?;
    only_taken(&before.snapshot, taken)?;
    Ok(before)
}

/// Tracks whose file an earlier removal may already have deleted: the
/// targets of every batch the phone did not apply and of the outstanding
/// one. Empty without a journal.
pub(crate) fn taken(journal_path: &Path, identity: &Identity) -> Result<BTreeSet<u64>> {
    if !journal_path.try_exists()? {
        return Ok(BTreeSet::new());
    }
    let journal = Journal::open_read_only(journal_path, identity)?;
    let mut tracks = journal.not_applied_removal_tracks()?;
    if let Some(batch) = journal.outstanding_removal_batch()? {
        tracks.extend(batch.targets.iter().map(|t| t.track));
    }
    Ok(tracks)
}

/// Every file missing from the snapshot is one an earlier removal took.
pub(crate) fn only_taken(snapshot: &Snapshot, taken: &BTreeSet<u64>) -> Result<()> {
    if let Some(track) = snapshot.index.gone().iter().find(|t| !taken.contains(t)) {
        bail!(
            "track {track:016x} has lost its audio file on the phone, and no removal of it was cut short; nothing was sent"
        );
    }
    Ok(())
}

/// The same capture for a caller that does not know the device yet: the
/// journal identity comes from the snapshot's own device and preferences.
pub(crate) async fn capture_fresh(
    directory: &Path,
    control: &Control,
) -> Result<(Identity, Before)> {
    capture(directory, None, Absent::NONE, control).await
}

/// Rows without audio that `absent` allows are accepted; every other track
/// must hold its audio.
async fn capture(
    directory: &Path,
    identity: Option<&Identity>,
    absent: Absent<'_>,
    control: &Control,
) -> Result<(Identity, Before)> {
    create_trace(directory)?;
    let snapshot_dir = directory.join("snapshot");
    // The run's latest complete snapshot, copied here, stands in for a new
    // one when the fresh state capture below finds the very same catalogue.
    let reused = match device::remembered_snapshot() {
        Some(latest) => match copy_snapshot(&latest, &snapshot_dir)
            .and_then(|()| snapshot::open_allowing(&snapshot_dir, absent))
        {
            Ok(snapshot) => Some(snapshot),
            Err(_) if snapshot_dir.exists() => {
                fs::rename(&snapshot_dir, directory.join("snapshot-unusable"))?;
                None
            }
            Err(_) => None,
        },
        None => None,
    };
    let fresh = reused.is_none();
    let mut snapshot = match reused {
        Some(snapshot) => snapshot,
        None => {
            audio_capture::capture_allowing(&snapshot_dir, absent, control, device::provider)
                .await?;
            snapshot::open_allowing(&snapshot_dir, absent)?
        }
    };
    control.check()?;
    let observed = Identity::for_saved_device(
        snapshot.identity.device_identifier_sha256,
        &identity::parse(&snapshot.raw_preferences)?,
    )?;
    if let Some(identity) = identity {
        ensure!(
            observed.device_digest() == identity.device_digest()
                && observed.library_digest() == identity.library_digest(),
            "the connected phone or library differs from the journal"
        );
    }
    let current = current::capture_with_artwork_controlled(
        &directory.join("current"),
        &observed,
        control.deadline,
        Arc::clone(&control.cancelled),
    )
    .await?;
    let same = |snapshot: &Snapshot| {
        let (a, b) = (
            current.catalog.source_hashes(),
            snapshot.catalog.source_hashes(),
        );
        a.database == b.database
            && a.database_bytes == b.database_bytes
            && a.wal == b.wal
            && a.wal_bytes == b.wal_bytes
    };
    if !fresh && !same(&snapshot) {
        // The phone changed since the reused snapshot: index it again.
        fs::rename(&snapshot_dir, directory.join("snapshot-stale"))?;
        audio_capture::capture_allowing(&snapshot_dir, absent, control, device::provider).await?;
        snapshot = snapshot::open_allowing(&snapshot_dir, absent)?;
        let again = Identity::for_saved_device(
            snapshot.identity.device_identifier_sha256,
            &identity::parse(&snapshot.raw_preferences)?,
        )?;
        ensure!(
            again.device_digest() == observed.device_digest()
                && again.library_digest() == observed.library_digest(),
            "the connected phone or library differs from the reused snapshot"
        );
    }
    ensure!(
        same(&snapshot),
        "the phone's catalogue changed between the snapshot and the state capture; retry"
    );
    let sql = materialize(&snapshot, &directory.join("sql"), control)?;
    Ok((
        observed,
        Before {
            directory: directory.to_owned(),
            snapshot,
            current,
            sql,
        },
    ))
}

/// Copy a complete snapshot directory into a new private one, files keeping
/// their modes, everything synchronized.
fn copy_snapshot(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_snapshot(&entry.path(), &target)?;
        } else {
            ensure!(kind.is_file(), "snapshot holds a non-regular file");
            fs::copy(entry.path(), &target)?;
            fs::File::open(&target)?.sync_all()?;
        }
    }
    fs::File::open(to)?.sync_all()?;
    Ok(())
}

/// The complete typed SQL of a saved snapshot, for selection and checks.
pub(crate) fn materialize(
    snapshot: &Snapshot,
    output: &Path,
    control: &Control,
) -> Result<reader::Snapshot> {
    let hashes = snapshot.catalog.source_hashes();
    let wal = snapshot.catalog_wal.as_deref();
    let mut budget =
        Budget::new_cancellable_until(control.deadline.into_std(), Arc::clone(&control.cancelled));
    reader::materialize(
        Capture::bound(&snapshot.catalog_database, wal, hashes)?,
        output,
        &mut budget,
    )
}

/// Each selected track checked against the removal model on `sql`, the
/// snapshot's typed catalogue, and each batch's operation built from
/// `snapshot`, so a refusal shows before anything is sent; every batch checks
/// both again against fresh state. Returns one step per track (its blockers)
/// and the batches whose operation could not be built.
pub(crate) fn qualify_selection(
    snapshot: &Snapshot,
    sql: &reader::Snapshot,
    selected: &[u64],
    batch_size: usize,
) -> Result<(Vec<serde_json::Value>, Vec<serde_json::Value>)> {
    let store = sql.table("item_store")?;
    let steps = selected
        .iter()
        .map(|track| {
            let blockers = match typed::unique(store, "sync_id", Cell::Integer(*track as i64))
                .and_then(|row| typed::int(store, row, "item_pid"))
                .and_then(|item| removal_sql::qualify(&sql.tables, &[item]))
            {
                Ok(()) => Vec::new(),
                Err(error) => vec![format!("{error:#}")],
            };
            json!({"track_pid":track,"blockers":blockers})
        })
        .collect();
    let batches = selected
        .chunks(batch_size.max(1))
        .enumerate()
        .filter_map(|(n, chunk)| {
            mutation::delete_many(&snapshot.catalog, chunk, SystemTime::now())
                .err()
                .map(|error| json!({"batch":n + 1,"blocker":format!("{error:#}")}))
        })
        .collect();
    Ok((steps, batches))
}

/// Exact journal targets for these tracks, in order: item, path, size and
/// full hash from the complete index, and the active binding if owned. A
/// track whose file is already gone has no file to remove.
pub(crate) fn targets(
    before: &Before,
    journal: &Journal,
    tracks: &[u64],
) -> Result<Vec<RemovalTarget>> {
    let mut owned = std::collections::BTreeMap::new();
    for active in journal.active_bindings()? {
        owned.insert(
            active.binding.content.track_pid,
            (active.reference.id, active.reference.version),
        );
    }
    let catalog = &before.snapshot.catalog;
    let index = &before.snapshot.index;
    tracks
        .iter()
        .map(|track| {
            let entry = catalog
                .tracks()
                .iter()
                .find(|t| t.sync_id == *track)
                .with_context(|| format!("track {track:016x} is not in the catalogue"))?;
            let (path, file_bytes, file_sha256) = match &entry.afc_path {
                None => (None, None, None),
                Some(_) if index.gone().contains(track) => (None, None, None),
                Some(path) => {
                    let (bytes, sha) = if let Some(audio) = index.tracks().get(track) {
                        (
                            audio.fingerprint.file_bytes,
                            audio.fingerprint.full_file_sha256,
                        )
                    } else if let Some(audio) = index.other_audio().get(track) {
                        (audio.file_bytes, audio.full_file_sha256)
                    } else {
                        bail!("track {track:016x} has a path but no complete index entry")
                    };
                    (Some(path.clone()), Some(bytes), Some(sha))
                }
            };
            Ok(RemovalTarget {
                track: *track,
                item_pid: before.item_pid(*track)?,
                path,
                file_bytes,
                file_sha256,
                binding: owned.get(track).copied(),
            })
        })
        .collect()
}

/// What a batch ended as.
pub(crate) struct Settled {
    pub(crate) batch: BatchId,
    pub(crate) state: BatchState,
    pub(crate) evidence: PathBuf,
    /// Tracks removed.
    pub(crate) removed: usize,
    /// Playlists deleted.
    pub(crate) deleted_playlists: usize,
}

/// Why a batch removes its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Tracks the user chose to remove.
    Remove,
    /// The row a Started Insert left without audio.
    CompensateInsert(AttemptId),
    /// The rows a Partial insert batch left without audio.
    CompensateInsertBatch(InsertBatchId),
}

/// Prepare, send and settle one batch removing `tracks` (in this order) from
/// the exact `before` state. Errors before Started leave nothing behind but a
/// Prepared batch that is cancelled here; after Started the batch is settled
/// by observation or left Started for `settle`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    journal: &mut Journal,
    identity: &Identity,
    before: &Before,
    tracks: &[u64],
    purpose: Purpose,
    coordination: Option<CoordinationId>,
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    control.check()?;
    ensure!(
        journal.outstanding_removal_batch()?.is_none(),
        "an earlier removal batch is unfinished; run resume first"
    );
    let items = tracks
        .iter()
        .map(|t| before.item_pid(*t))
        .collect::<Result<Vec<_>>>()?;
    removal_sql::qualify(&before.sql.tables, &items)?;
    let targets = targets(before, journal, tracks)?;
    let current = &before.current;
    let operation = match purpose {
        Purpose::Remove => mutation::delete_many(&current.catalog, tracks, SystemTime::now())?,
        Purpose::CompensateInsert(_) | Purpose::CompensateInsertBatch(_) => {
            mutation::delete_many_unfilled(&current.catalog, tracks, SystemTime::now())?
        }
    };
    let base_anchor = before.anchor();
    let evidence = &before.directory;
    let input = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.removal-batch-input",
        "base_anchor":base_anchor,
        "operation_sha256":hex(&sha(&operation)),
        "before_index_sha256":hex(&before.snapshot.index_receipt_sha256),
        "before_current_sha256":hex(&current.receipt_sha256),
        "artwork_database_sha256":hex(&sha(current.artwork_db.as_deref().unwrap_or_default())),
        "host_name":host_name,
        "compensates":match purpose {
            Purpose::Remove => serde_json::Value::Null,
            Purpose::CompensateInsert(id) => json!({"attempt":id.as_i64()}),
            Purpose::CompensateInsertBatch(id) => json!({"insert_batch":id.as_i64()}),
        },
        "targets":targets.iter().map(|t| json!({
            "track":format!("{:016x}",t.track),"item_pid":t.item_pid,"path":t.path,
            "file_bytes":t.file_bytes,"file_sha256":t.file_sha256.map(|h| hex(&h)),
            "binding":t.binding.map(|(id,version)| json!({"id":id.as_i64(),"version":version})),
        })).collect::<Vec<_>>(),
    }))?;
    durable_write(evidence, "input.json", &input)?;
    durable_write(evidence, "operation.plist", &operation)?;
    let input_sha = sha(&input);
    let id = journal.prepare_removal_batch(&RemovalIntent {
        coordination,
        compensates: match purpose {
            Purpose::CompensateInsert(id) => Some(id),
            _ => None,
        },
        compensates_insert_batch: match purpose {
            Purpose::CompensateInsertBatch(id) => Some(id),
            _ => None,
        },
        operation: operation.clone(),
        base_anchor,
        before_current: current.receipt_sha256,
        before_index: before.snapshot.index_receipt_sha256,
        input_receipt: input_sha,
        evidence: evidence.clone(),
        targets,
        playlists: Vec::new(),
    })?;
    let requests = match purpose {
        Purpose::Remove => Requests::Refuse(tracks[0]),
        _ => Requests::Unfilled(tracks),
    };
    send_and_settle(
        journal, identity, before, id, &operation, requests, input_sha, host_name, control,
    )
    .await
}

/// Prepare, send and settle one batch deleting `playlists` (in this order)
/// from the exact `before` state, under the same rules as `run`. Each must be
/// an empty ordinary playlist (`removal_sql::qualify_deletion`) named as in
/// `before`. The session names no track, so every asset request refuses.
pub(crate) async fn run_playlists(
    journal: &mut Journal,
    identity: &Identity,
    before: &Before,
    playlists: &[PlaylistTarget],
    coordination: CoordinationId,
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    control.check()?;
    ensure!(
        journal.outstanding_removal_batch()?.is_none(),
        "an earlier removal batch is unfinished; run resume first"
    );
    let pids: Vec<i64> = playlists.iter().map(|p| p.container as i64).collect();
    removal_sql::qualify_deletion(&before.sql.tables, &pids)?;
    let containers = before.sql.table("container")?;
    for playlist in playlists {
        let row = typed::unique(
            containers,
            "container_pid",
            Cell::Integer(playlist.container as i64),
        )?;
        ensure!(
            containers.at(row, "name")?.text()? == playlist.name,
            "playlist {:016x} is no longer named {:?}",
            playlist.container,
            playlist.name
        );
    }
    let current = &before.current;
    let containers: Vec<u64> = playlists.iter().map(|p| p.container).collect();
    let operation = mutation::delete_playlists(&current.catalog, &containers, SystemTime::now())?;
    let base_anchor = before.anchor();
    let evidence = &before.directory;
    let input = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.playlist-removal-input",
        "base_anchor":base_anchor,
        "operation_sha256":hex(&sha(&operation)),
        "before_index_sha256":hex(&before.snapshot.index_receipt_sha256),
        "before_current_sha256":hex(&current.receipt_sha256),
        "artwork_database_sha256":hex(&sha(current.artwork_db.as_deref().unwrap_or_default())),
        "host_name":host_name,
        "playlists":playlists.iter().map(|p| json!({
            "container":format!("{:016x}",p.container),"name":p.name,
        })).collect::<Vec<_>>(),
    }))?;
    durable_write(evidence, "input.json", &input)?;
    durable_write(evidence, "operation.plist", &operation)?;
    let input_sha = sha(&input);
    let id = journal.prepare_removal_batch(&RemovalIntent {
        coordination: Some(coordination),
        compensates: None,
        compensates_insert_batch: None,
        operation: operation.clone(),
        base_anchor,
        before_current: current.receipt_sha256,
        before_index: before.snapshot.index_receipt_sha256,
        input_receipt: input_sha,
        evidence: evidence.clone(),
        targets: Vec::new(),
        playlists: playlists.to_vec(),
    })?;
    send_and_settle(
        journal,
        identity,
        before,
        id,
        &operation,
        Requests::None,
        input_sha,
        host_name,
        control,
    )
    .await
}

/// Send Prepared batch `id` and settle it by observation. A failure before
/// Started cancels it; after Started it is settled or left for `settle`.
#[allow(clippy::too_many_arguments)]
async fn send_and_settle(
    journal: &mut Journal,
    identity: &Identity,
    before: &Before,
    id: BatchId,
    operation: &[u8],
    requests: Requests<'_>,
    input_sha: [u8; 32],
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    let send = before.directory.join("send");
    let wire = match send_metadata_only(
        before,
        operation,
        requests,
        input_sha,
        host_name,
        &send,
        control,
        |ready, signature| journal.start_removal_batch(id, ready, signature),
    )
    .await
    {
        Sent::Unsent(error) => {
            journal.cancel_removal_batch(id)?;
            return Err(error);
        }
        Sent::Started(wire) => wire.and_then(|(ready, terminal, operation_sha)| {
            durable_write(
                &send,
                "wire.json",
                &serde_json::to_vec_pretty(&json!({
                    "schema":"rustunes.removal-batch-wire","batch":id.as_i64(),
                    "ready_sha256":hex(&ready),"terminal_sha256":hex(&terminal),
                    "operation_sha256":hex(&operation_sha),
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
    let batch = journal.removal_batch(id)?;
    let mut attempt = 1;
    let observed = loop {
        match settle(journal, identity, &batch, Window::SameRun, control).await {
            Err(error) if attempt < SETTLE_ATTEMPTS && control.check().is_ok() => {
                let _ = durable_write(
                    &send,
                    &format!("observation-error-{attempt}.txt"),
                    format!("{error:#}\nBatch {} is observed again.\n", id.as_i64()).as_bytes(),
                );
                attempt += 1;
                tokio::time::sleep(SETTLE_PAUSE).await;
            }
            result => break result,
        }
    };
    match (observed, wire) {
        (Ok(settled), _) => Ok(settled),
        (Err(error), Ok(())) => Err(error.context(format!(
            "removal batch {} is Started after {SETTLE_ATTEMPTS} observations; the next run (or resume) observes it again",
            id.as_i64()
        ))),
        (Err(error), Err(wire)) => Err(error.context(format!(
            "removal batch {} is Started after a transport error ({wire:#}) and {SETTLE_ATTEMPTS} observations; the next run (or resume) observes it again",
            id.as_i64()
        ))),
    }
}

/// Whether a batch attempt that failed or did not apply may be tried again
/// from fresh state: the run was not cancelled and nothing is left Started.
pub(crate) fn may_try_again(
    journal_path: &Path,
    identity: &Identity,
    control: &Control,
) -> Result<bool> {
    Ok(control.check().is_ok()
        && Journal::open_read_only(journal_path, identity)?
            .outstanding_removal_batch()?
            .is_none())
}

impl StartedOperation for StartedBatch {
    fn operation(&self) -> &[u8] {
        StartedBatch::operation(self)
    }
    fn signature(&self) -> &[u8; 21] {
        StartedBatch::signature(self)
    }
}

/// How far `send_metadata_only` got.
pub(crate) enum Sent {
    /// Nothing was written to the phone; the caller cancels its Prepared record.
    Unsent(anyhow::Error),
    /// Started was recorded; the wire result (ready receipt, terminal receipt,
    /// staged operation hash) or why the session failed. Only observation
    /// settles it.
    Started(Result<([u8; 32], [u8; 32], [u8; 32])>),
}

/// Which asset requests a metadata-only session refuses as its own.
#[derive(Clone, Copy)]
pub(crate) enum Requests<'a> {
    /// The first target is an installed track; a request for it refuses.
    Refuse(u64),
    /// Every target is a row without audio; a request for any of them refuses.
    Unfilled(&'a [u64]),
    /// The session names no track (a playlist deletion); every asset request
    /// refuses but eligible foreign artwork and Genius ones.
    None,
}

/// Connect, guard and sign one metadata-only `operation` against the exact
/// `before` state, then call `start`, which must durably record Started and
/// return the journal's copy of the bytes, and send them. The guards hold the
/// state files, ArtworkDB and an empty staging folder unchanged from `before`
/// through the Ready session. Own asset requests (`requests`) refuse.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_metadata_only<T: StartedOperation>(
    before: &Before,
    operation: &[u8],
    requests: Requests<'_>,
    input_receipt: [u8; 32],
    host_name: &str,
    send: &Path,
    control: &Control,
    start: impl FnOnce([u8; 32], [u8; 21]) -> Result<T>,
) -> Sent {
    let current = &before.current;
    let prepared = (|| {
        let artwork = current
            .artwork_db
            .clone()
            .context("a metadata-only session requires the copied ArtworkDB")?;
        ensure!(
            !artwork.is_empty() && artwork.len() as u64 <= MAX_ASSET,
            "ArtworkDB exceeds bound"
        );
        // A compensating batch's rows have no file location.
        let mut state = FreshState::for_unfilled(
            &current.device,
            &current.catalog,
            &current.raw_preferences,
            &current.plist_preferences,
            input_receipt,
            before.snapshot.index.unfilled(),
        )?;
        state.bind_copied_cdb(&current.cdb)?;
        ensure!(
            state.media_anchor() == before.anchor(),
            "session state anchor differs from the snapshot"
        );
        let templates = templates::build_from_copied_cdb(
            &current.cdb,
            &current.catalog,
            &current.library,
            host_name,
        )?;
        let session = session::prepare(
            templates.request,
            &templates.finished,
            current.library.library_id(),
            before.anchor(),
        )?;
        let operations = operation_budget(
            state.state_read_chunks()?,
            artwork.len() as u64,
            operation.len(),
        )?;
        create_trace(send)?;
        Ok((artwork, state, session, operations))
    })();
    let (artwork, state, session_prepared, operations) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Sent::Unsent(error),
    };
    let artwork_sha = sha(&artwork);
    let ancillary = state.ancillary();
    let policy = match match requests {
        Requests::Refuse(first) => ancillary.metadata(&(first as i64).to_string()),
        Requests::Unfilled(targets) => ancillary.metadata_unfilled(
            &targets
                .iter()
                .map(|t| (*t as i64).to_string())
                .collect::<Vec<_>>(),
        ),
        Requests::None => Ok(ancillary.metadata_without_tracks()),
    }
    .and_then(|policy| {
        policy.ensure_session(current.library.library_id(), before.anchor())?;
        Ok(policy)
    }) {
        Ok(policy) => policy,
        Err(error) => return Sent::Unsent(error),
    };
    let mut session = None;
    let mut afc = None;
    let ready = controlled(control, async {
        let provider = timeout_at(
            control
                .deadline
                .min(Instant::now() + Duration::from_secs(15)),
            device::provider(),
        )
        .await
        .context("discovery deadline")??;
        ensure!(
            provider.udid == current.device,
            "discovered device differs from the captured state"
        );
        let afc = afc.insert(
            Transfer::connect_bounded(
                &provider,
                send,
                control.deadline,
                operations,
                Arc::clone(&control.cancelled),
            )
            .await?,
        );
        guard(afc, &state, artwork.len() as u64, artwork_sha).await?;
        control.check()?;
        let live = session
            .insert(Session::connect(&provider, session_prepared, send, control.deadline).await?);
        guard(afc, &state, artwork.len() as u64, artwork_sha).await?;
        let signature = live.sign_metadata(operation)?;
        durable_write(send, "operation.cig", &signature)?;
        sync_trace(send)?;
        Ok((live.ready_receipt_sha256(), signature))
    })
    .await
    .and_then(|(ready, signature)| {
        control.check()?;
        Ok((ready, start(ready, signature)?))
    });
    let (ready, started) = match ready {
        Ok(ready) => ready,
        Err(error) => {
            let _ = durable_write(
                send,
                "error.txt",
                format!("{error:#}\nNothing was written to the phone.\n").as_bytes(),
            );
            return Sent::Unsent(error);
        }
    };
    // From here the record is Started: nothing on the phone changed yet, and
    // only observation may settle it.
    let (Some(mut session), Some(mut afc)) = (session, afc) else {
        return Sent::Started(Err(anyhow::anyhow!("transport lost before Started")));
    };
    let wire = controlled(control, async {
        session.stage_metadata(&mut afc, &started).await?;
        control.check()?;
        session.finish_metadata().await?;
        session.wait_metadata_only_with_policy(&policy).await?;
        Ok((
            ready,
            session.terminal_receipt_sha256()?,
            sha(started.operation()),
        ))
    })
    .await;
    drop(session);
    drop(afc);
    Sent::Started(wire)
}

/// Remove `tracks` in order, at most `batch_size` per session. Each batch is
/// captured fresh under `run/batch-NNN` and settled before the next starts;
/// `progress(removed, total)` runs after each one. A batch the phone rolled
/// back (NotApplied), or one that failed before it was sent, is tried once
/// more from fresh state (`run/batch-NNN-attempt-2`); a second such outcome,
/// or a batch left Started, stops the run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn remove_all(
    journal_path: &Path,
    identity: &Identity,
    tracks: &[u64],
    batch_size: usize,
    coordination: Option<CoordinationId>,
    run_directory: &Path,
    host_name: &str,
    control: impl Fn() -> Control,
    mut progress: impl FnMut(&Settled, usize, usize) -> Result<()>,
) -> Result<Vec<Settled>> {
    ensure!(batch_size > 0, "batch size must be positive");
    let mut settled = Vec::new();
    let mut removed = 0;
    for (number, chunk) in tracks.chunks(batch_size).enumerate() {
        crate::engine::observe(crate::engine::Event::RemovalBatchStarted {
            number: number + 1,
            batches: tracks.len().div_ceil(batch_size),
            tracks: chunk.len(),
            removed,
            total: tracks.len(),
        })?;
        let mut attempt = 1;
        let batch = loop {
            let control = control();
            let directory = run_directory.join(match attempt {
                1 => format!("batch-{:03}", number + 1),
                n => format!("batch-{:03}-attempt-{n}", number + 1),
            });
            let result = async {
                let taken = taken(journal_path, identity)?;
                let before = capture_for_removal(&directory, identity, &taken, &control).await?;
                let mut journal = Journal::open(journal_path, identity)?;
                run(
                    &mut journal,
                    identity,
                    &before,
                    chunk,
                    Purpose::Remove,
                    coordination,
                    host_name,
                    &control,
                )
                .await
            }
            .await;
            match result {
                Ok(batch) if batch.state == BatchState::Verified => break batch,
                Ok(_) | Err(_)
                    if attempt < BATCH_ATTEMPTS
                        && may_try_again(journal_path, identity, &control)? =>
                {
                    attempt += 1
                }
                Ok(batch) => bail!(
                    "removal batch {} did not apply on the phone, after {attempt} attempts; {removed} of {} tracks were removed",
                    batch.batch.as_i64(),
                    tracks.len()
                ),
                Err(error) => {
                    return Err(error.context(format!(
                        "{removed} of {} tracks were removed before this batch",
                        tracks.len()
                    )));
                }
            }
        };
        removed += batch.removed;
        progress(&batch, removed, tracks.len())?;
        settled.push(batch);
    }
    Ok(settled)
}

/// Capture a fresh complete state under `directory` and remove the rows an
/// unfinished insertion left without audio, as one compensating batch. Each
/// row must still be a library row with no file location and no asset, and
/// no file may exist at any of the insertion's `planned` permanent paths.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compensate(
    journal_path: &Path,
    identity: &Identity,
    directory: &Path,
    tracks: &[u64],
    planned: &[String],
    purpose: Purpose,
    coordination: Option<CoordinationId>,
    host_name: &str,
    control: &Control,
) -> Result<Settled> {
    ensure!(
        purpose != Purpose::Remove && !tracks.is_empty() && planned.len() == tracks.len(),
        "compensation needs the rows of an unfinished insertion"
    );
    let unfilled: BTreeSet<u64> = tracks.iter().copied().collect();
    let absent = Absent::unfilled(&unfilled);
    let (_, before) = capture(directory, Some(identity), absent, control).await?;
    ensure!(
        before.snapshot.index.unfilled() == &unfilled,
        "a row the unfinished insertion left now holds audio, or is gone; nothing was sent"
    );
    let library = identity::parse(&before.snapshot.raw_preferences)?;
    let trace = directory.join("planned-paths");
    create_trace(&trace)?;
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(30));
    let mut afc = read_only(
        identity,
        &library,
        &trace,
        deadline,
        planned.len() * 4 + 16,
        control,
    )
    .await?;
    for path in planned {
        ensure!(
            afc.stat(path).await?.is_none(),
            "a file exists at {path}, where the unfinished insertion planned its audio; nothing was sent"
        );
    }
    drop(afc);
    durable_write(
        directory,
        "planned-paths-absent.json",
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.compensation-planned-paths",
            "tracks":tracks.iter().map(|t| format!("{t:016x}")).collect::<Vec<_>>(),
            "absent":planned,
        }))?,
    )?;
    let mut journal = Journal::open(journal_path, identity)?;
    run(
        &mut journal,
        identity,
        &before,
        tracks,
        purpose,
        coordination,
        host_name,
        control,
    )
    .await
}

/// Observe a Started batch and settle it: Verified when the complete after
/// state is the modelled removal with survivors intact and target files gone;
/// NotApplied when nothing moved and nothing is staged but this batch's own
/// unfinished operation pair, which is removed first. A target's file may
/// already be gone from a session the phone rolled back: the catalogue
/// decides, and the proof lists those files. Otherwise it stays Started and
/// the error says why; the full SQL delta is kept.
pub(crate) async fn settle(
    journal: &mut Journal,
    identity: &Identity,
    batch: &BatchRecord,
    window: Window,
    control: &Control,
) -> Result<Settled> {
    ensure!(
        batch.state == BatchState::Started,
        "removal batch {} is not Started",
        batch.id.as_i64()
    );
    let evidence = &batch.evidence;
    // A compensating batch removes rows without audio; only those may lack it.
    let unfilled: BTreeSet<u64> = batch
        .targets
        .iter()
        .filter(|t| t.path.is_none())
        .map(|t| t.track)
        .collect();
    // A removal the phone rolled back may have deleted a target's file.
    let absent = Absent {
        unfilled: &unfilled,
        gone: batch.compensates.is_none() && batch.compensates_insert_batch.is_none(),
        ..Absent::NONE
    };
    let targeted: BTreeSet<u64> = batch.targets.iter().map(|t| t.track).collect();
    let before = snapshot::open_allowing(&evidence.join("snapshot"), absent)?;
    ensure!(
        before.index_receipt_sha256 == batch.before_index,
        "retained before snapshot differs from the journaled batch"
    );
    let run = next_directory(evidence, "observe")?;
    create_trace(&run)?;
    let library = identity::parse(&before.raw_preferences)?;
    let before_sql = materialize(&before, &run.join("sql-before"), control)?;
    let before_tables = &before_sql.tables;
    let items: Vec<i64> = batch.targets.iter().map(|t| t.item_pid).collect();
    let playlists: Vec<i64> = batch.playlists.iter().map(|p| p.container as i64).collect();
    let after_dir = run.join("snapshot");
    audio_capture::capture_allowing(&after_dir, absent, control, device::provider).await?;
    let after = snapshot::open_allowing(&after_dir, absent)?;
    ensure!(
        before.index.gone().is_subset(&targeted) && after.index.gone().is_subset(&targeted),
        "an audio file missing from the phone belongs to no target of batch {}; it stays Started",
        batch.id.as_i64()
    );
    let after_sql = materialize(&after, &run.join("sql-after"), control)?;
    let after_tables = &after_sql.tables;
    let budget =
        Budget::new_cancellable_until(control.deadline.into_std(), Arc::clone(&control.cancelled));
    if let Ok(delta) = reader::delta(&before_sql, &after_sql, &budget) {
        durable_write(&run, "sql-delta.json", &serde_json::to_vec(&delta)?)?;
    }
    let anchor = after.catalog.persisted_media_anchor();
    if anchor == batch.base_anchor {
        let unchanged = match (playlists.is_empty(), window) {
            (true, Window::SameRun) => {
                removal_sql::not_applied(before_tables, after_tables, &items)?
            }
            (true, Window::LaterRun) => {
                removal_sql::not_applied_later(before_tables, after_tables, &items)?
            }
            // Listening never touches an empty playlist.
            (false, _) => {
                removal_sql::deletion_not_applied(before_tables, after_tables, &playlists)?
            }
        };
        ensure!(
            unchanged,
            "the anchor did not move but the catalogue changed; batch {} stays Started",
            batch.id.as_i64()
        );
        let staged = staged_names(identity, &library, &run, control).await?;
        let staged_removed = remove_unfinished(
            identity,
            &library,
            &run,
            &staged,
            stale_staging::Unfinished {
                send: &evidence.join("send"),
                operation: &batch.operation,
                signature: batch.signature,
            },
            batch.id.as_i64(),
            control,
        )
        .await?;
        let proof = serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.removal-batch-not-applied","batch":batch.id.as_i64(),
            "anchor":anchor,"after_index_sha256":hex(&after.index_receipt_sha256),
            "targets_present":items,"playlists_present":playlists,"staging_empty":true,
            "staged_removed":staged_removed,
            "target_files_gone":after.index.gone().iter().map(|t| format!("{t:016x}")).collect::<Vec<_>>(),
        }))?;
        durable_write(&run, "not-applied.json", &proof)?;
        journal.mark_removal_batch_not_applied(batch.id, sha(&proof))?;
        return Ok(Settled {
            batch: batch.id,
            state: BatchState::NotApplied,
            evidence: run,
            removed: 0,
            deleted_playlists: 0,
        });
    }
    let unmodelled = || {
        format!(
            "the phone's catalogue is not the modelled removal; batch {} stays Started and {} keeps the delta",
            batch.id.as_i64(),
            run.display()
        )
    };
    // A later run: the phone may have been played since the session.
    let between_runs =
        |expected: &mut removal_sql::Tables, observed: &removal_sql::Tables, floor: i64| {
            between_runs::absorb_above(expected, observed, &budget, floor)
        };
    let removal = match (playlists.is_empty(), window) {
        (true, Window::SameRun) => serde_json::to_value(
            removal_sql::verify(before_tables, after_tables, &items, batch.base_anchor)
                .with_context(unmodelled)?,
        )?,
        (true, Window::LaterRun) => serde_json::to_value(
            removal_sql::verify_later(
                before_tables,
                after_tables,
                &items,
                batch.base_anchor,
                &between_runs,
            )
            .with_context(unmodelled)?,
        )?,
        (false, Window::SameRun) => serde_json::to_value(
            removal_sql::verify_deletion(
                before_tables,
                after_tables,
                &playlists,
                batch.base_anchor,
            )
            .with_context(unmodelled)?,
        )?,
        (false, Window::LaterRun) => serde_json::to_value(
            removal_sql::verify_deletion_later(
                before_tables,
                after_tables,
                &playlists,
                batch.base_anchor,
                &between_runs,
            )
            .with_context(unmodelled)?,
        )?,
    };
    survivors_unchanged(&before, &after, batch)?;
    let remaining = wait_absent(identity, &library, &run, batch, control).await?;
    ensure!(
        remaining.is_empty(),
        "target files are still on the phone after {} s: {}; batch {} stays Started",
        ASSET_WAIT.as_secs(),
        remaining.join(", "),
        batch.id.as_i64()
    );
    let mut receipt = json!({
        "schema":"rustunes.removal-batch-verification","batch":batch.id.as_i64(),
        "base_anchor":batch.base_anchor,"verified_anchor":anchor,
        "before_index_sha256":hex(&batch.before_index),
        "after_index_sha256":hex(&after.index_receipt_sha256),
        "removal":removal,"survivors_unchanged":true,"target_files_absent":true,
    });
    allocation_changed(&mut receipt, &before, &after);
    let receipt = serde_json::to_vec_pretty(&receipt)?;
    durable_write(&run, "verification.json", &receipt)?;
    journal.settle_removal_batch(batch.id, sha(&receipt), anchor)?;
    Ok(Settled {
        batch: batch.id,
        state: BatchState::Verified,
        evidence: run,
        removed: batch.targets.len(),
        deleted_playlists: batch.playlists.len(),
    })
}

/// Every surviving indexed file is as before (its allocation aside,
/// `TrackAudio::same_file`), no target is indexed, and no file appeared.
fn survivors_unchanged(before: &Snapshot, after: &Snapshot, batch: &BatchRecord) -> Result<()> {
    let removed: BTreeSet<u64> = batch.targets.iter().map(|t| t.track).collect();
    let tracks = |s: &Snapshot| -> Vec<(u64, String)> {
        s.index
            .tracks()
            .iter()
            .filter(|(id, _)| !removed.contains(id))
            .map(|(id, t)| (*id, t.afc_path.clone()))
            .collect()
    };
    ensure!(
        tracks(before) == tracks(after),
        "surviving ALAC files differ after the removal"
    );
    for (id, audio) in before.index.tracks() {
        if !removed.contains(id) {
            ensure!(
                after
                    .index
                    .tracks()
                    .get(id)
                    .is_some_and(|a| a.same_file(audio)),
                "surviving track {id:016x} changed"
            );
        }
    }
    for (id, audio) in before.index.other_audio() {
        if !removed.contains(id) {
            ensure!(
                after
                    .index
                    .other_audio()
                    .get(id)
                    .is_some_and(|a| a.same_file(audio)),
                "surviving track {id:016x} changed"
            );
        }
    }
    let after_ids = after
        .index
        .tracks()
        .keys()
        .chain(after.index.other_audio().keys());
    for id in after_ids {
        ensure!(
            !removed.contains(id),
            "removed track {id:016x} is still indexed"
        );
        ensure!(
            before.index.tracks().contains_key(id) || before.index.other_audio().contains_key(id),
            "track {id:016x} appeared during the removal"
        );
    }
    Ok(())
}

/// Poll every target file until it is absent or `ASSET_WAIT` passes.
pub(crate) async fn wait_absent(
    identity: &Identity,
    library: &LibraryIdentity,
    run: &Path,
    batch: &BatchRecord,
    control: &Control,
) -> Result<Vec<String>> {
    let paths: Vec<String> = batch
        .targets
        .iter()
        .filter_map(|t| t.path.clone())
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let trace = run.join("absence");
    create_trace(&trace)?;
    let deadline = control.deadline.min(Instant::now() + ASSET_WAIT);
    let mut afc = read_only(
        identity,
        library,
        &trace,
        deadline,
        paths.len() * 200,
        control,
    )
    .await?;
    let mut remaining = paths;
    loop {
        let mut still = Vec::new();
        for path in remaining {
            if afc.stat(&path).await?.is_some() {
                still.push(path);
            }
        }
        remaining = still;
        if remaining.is_empty() || Instant::now() + Duration::from_secs(2) >= deadline {
            return Ok(remaining);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        control.check()?;
    }
}

/// `stale_staging::remove_unfinished` for batch `id`, which stays Started
/// when the staged files are not provably its own unfinished pair.
pub(crate) async fn remove_unfinished(
    identity: &Identity,
    library: &LibraryIdentity,
    run: &Path,
    staged: &[String],
    record: stale_staging::Unfinished<'_>,
    id: i64,
    control: &Control,
) -> Result<Vec<String>> {
    stale_staging::remove_unfinished(identity, library, run, staged, record, control)
        .await
        .with_context(|| {
            format!(
                "the operation is still staged on the phone ({}); batch {id} stays Started",
                staged.join(", ")
            )
        })
}

pub(crate) async fn staged_names(
    identity: &Identity,
    library: &LibraryIdentity,
    run: &Path,
    control: &Control,
) -> Result<Vec<String>> {
    let trace = run.join("staging");
    create_trace(&trace)?;
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(30));
    let mut afc = read_only(identity, library, &trace, deadline, 16, control).await?;
    Ok(afc
        .names(STAGING)
        .await?
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect())
}

async fn read_only(
    identity: &Identity,
    library: &LibraryIdentity,
    trace: &Path,
    deadline: Instant,
    operations: usize,
    control: &Control,
) -> Result<Transfer> {
    let provider = timeout_at(deadline, device::provider())
        .await
        .context("discovery deadline")??;
    ensure!(
        Identity::new(&provider.udid, library)?.device_digest() == identity.device_digest(),
        "discovered device differs from the journal"
    );
    Transfer::connect_bounded(
        &provider,
        trace,
        deadline,
        operations.min(MAX_BOUNDED_OPS),
        Arc::clone(&control.cancelled),
    )
    .await
}

/// Unchanged state and ArtworkDB, an empty staging namespace, all stable
/// across this combined readback.
async fn guard(
    afc: &mut Transfer,
    state: &FreshState,
    artwork_bytes: u64,
    artwork_sha: [u8; 32],
) -> Result<()> {
    let mut before = Vec::new();
    for name in STATE_FILES {
        before.push(afc.stat(name).await?);
    }
    state.verify_state(afc).await?;
    regular_size(
        before[6].as_ref().context("ArtworkDB missing")?,
        artwork_bytes,
    )?;
    ensure!(
        afc.read_hash(ARTWORK_DB, artwork_bytes, MAX_ASSET).await? == artwork_sha,
        "ArtworkDB changed since the removal was prepared"
    );
    afc.require_directory(STAGING).await?;
    ensure!(
        afc.names(STAGING)
            .await?
            .iter()
            .all(|name| name == "." || name == ".."),
        "the staging folder holds leftovers; nothing was removed"
    );
    for path in [METADATA, SIGNATURE] {
        afc.require_absent(path).await?;
    }
    for (name, previous) in STATE_FILES.into_iter().zip(before) {
        ensure!(
            stable(previous) == stable(afc.stat(name).await?),
            "the phone's state changed during the guard readback"
        );
    }
    Ok(())
}

fn stable(attrs: Option<Attributes>) -> Option<Attributes> {
    attrs.map(|mut attrs| {
        attrs.remove("st_atime");
        attrs
    })
}

fn operation_budget(
    state_chunks: u64,
    artwork_bytes: u64,
    operation_bytes: usize,
) -> Result<usize> {
    ensure!(
        operation_bytes > 0
            && operation_bytes <= music_protocol::protocol::signing::MAX_OPERATION_BYTES,
        "removal operation exceeds bound"
    );
    // Two complete guards, write and readback of metadata and signature, and a
    // generous margin: exhaustion after Started strands a staged operation.
    let operations = state_chunks
        .checked_add(artwork_bytes.div_ceil(CHUNK))
        .and_then(|n| n.checked_add(64))
        .and_then(|n| n.checked_mul(2))
        .and_then(|n| n.checked_add((operation_bytes as u64).div_ceil(CHUNK) * 4 + 512))
        .context("removal operation budget overflow")?;
    let operations = usize::try_from(operations)?;
    ensure!(
        operations <= MAX_BOUNDED_OPS,
        "removal AFC allowance exceeds bound"
    );
    Ok(operations)
}

/// The session takes no cancellation token; dropping the future closes both
/// transports and leaves a Started record for observation.
pub(crate) async fn controlled<T>(
    control: &Control,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    control.check()?;
    let cancellation = async {
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if let Err(error) = control.check() {
                return error;
            }
        }
    };
    tokio::select! {
        biased;
        error = cancellation => Err(error),
        result = timeout_at(control.deadline, future) => result.context("session deadline")?,
    }
}

pub(crate) fn next_directory(parent: &Path, prefix: &str) -> Result<PathBuf> {
    for n in 1..10_000 {
        let path = parent.join(format!("{prefix}-{n:03}"));
        if !path.try_exists()? {
            return Ok(path);
        }
    }
    bail!(
        "too many observation directories under {}",
        parent.display()
    )
}

/// Name in a batch receipt the files whose allocation alone changed
/// (`DeviceAudioIndex::allocation_changed`), if any.
pub(crate) fn allocation_changed(
    receipt: &mut serde_json::Value,
    before: &Snapshot,
    after: &Snapshot,
) {
    let ids = before.index.allocation_changed(&after.index);
    if !ids.is_empty() {
        receipt["allocation_changed"] = json!(
            ids.iter()
                .map(|id| format!("{id:016x}"))
                .collect::<Vec<_>>()
        );
    }
}

pub(crate) fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
