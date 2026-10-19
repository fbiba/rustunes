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
