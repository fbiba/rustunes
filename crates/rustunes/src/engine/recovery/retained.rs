//! Close provably completed journal intents without replaying phone operations.
//! Only the exact prepared revision, original signed session, preserved before
//! catalog and fresh readbacks can complete Started.

use crate::engine::execution::device::DeviceProvider;
use crate::{
    engine::execution::insert,
    engine::execution::native_insert::{
        BoundSource, Control, FreshState, create_trace, durable_write,
    },
    engine::planning::collections::{CandidateIds, Planner},
    engine::state::current::Current,
    engine::state::journal::{
        Attempt, AttemptKind, AttemptState, CompletionEvidence, Identity, Journal, StartEvidence,
    },
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::artwork_update;
use music_protocol::catalogue::artwork_verification;
use music_protocol::catalogue::identity;
use music_protocol::catalogue::insert_verification::{self, ExpectedInsert};
use music_protocol::catalogue::sqlite::{self, Catalog};
use music_protocol::media::artwork::Cover;
use music_protocol::media::fingerprint;
use music_protocol::media::metadata;
use music_protocol::protocol::ancillary::EligibleSnapshot;
use music_protocol::protocol::signing;
use music_protocol::protocol::templates;
use music_protocol::protocol::transfer::{MAX_AUDIO, MAX_BOUNDED_OPS, Transfer, hex};
use plist::{Dictionary, Value};
use serde::Serialize;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    future::Future,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, timeout_at};

const SMALL: u64 = 1024 * 1024;
const FRAME_CAP: u64 = 16 * 1024 * 1024;
const MAX_PLAN: u64 = 32 * 1024 * 1024;

pub(crate) mod playback;

mod ordinary_sql;

pub struct Options<'a> {
    pub run_dir: &'a Path,
    pub trace: &'a Path,
    /// Exact saved fresh Current, supplied by ordinary recovery capture.
    pub current_state_dir: &'a Path,
}

#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub status: &'static str,
    pub attempt_id: Option<String>,
    pub track_id: Option<String>,
    pub reason: Option<String>,
    pub evidence_sha256: Option<String>,
    pub journal_changed: bool,
    pub phone_writes: u32,
    pub replay_attempted: bool,
    pub reservations_retained: bool,
}
fn report(status: &'static str, attempt: Option<&Attempt>) -> Report {
    Report {
        schema: "rustunes.recovery",
        status,
        attempt_id: attempt.map(|a| a.id.as_i64().to_string()),
        track_id: attempt.map(|a| format!("{:016x}", a.prepared.content.track_pid)),
        reason: None,
        evidence_sha256: None,
        journal_changed: false,
        phone_writes: 0,
        replay_attempted: false,
        reservations_retained: true,
    }
}

/// Only a Started insert or cover of an ALAC track reaches read-only recovery.
/// Every other outstanding attempt is reported locally.
pub(crate) enum Dispatch {
    Local(Report),
    Started(Box<Attempt>),
}

pub(crate) fn dispatch(journal: &Journal) -> Result<Dispatch> {
    let Some(a) = journal.outstanding()? else {
        return Ok(Dispatch::Local(report("no_outstanding", None)));
    };
    if a.state == AttemptState::ReconciliationRequired && a.prepared.content.source.is_alac() {
        return Ok(Dispatch::Started(Box::new(a)));
    }
    let mut out = report("uncertain", Some(&a));
    out.reason = Some(if a.state == AttemptState::ReconciliationRequired {
        format!(
            "{} insertion recovery is unqualified. Preserve Started evidence; no discovery, retry, replay or completion.",
            a.prepared.content.source.format().label()
        )
    } else {
        format!(
            "Attempt {} was prepared but never started. Cancel it explicitly with --cancel-prepared {} before creating a fresh plan.",
            a.id.as_i64(),
            a.id.as_i64()
        )
    });
    Ok(Dispatch::Local(out))
}

/// Explicit local cancellation only. A Started attempt can never pass this gate.
pub fn cancel_prepared(
    journal: &mut Journal,
    identity: &Identity,
    expected_attempt: i64,
    trace: &Path,
) -> Result<Report> {
    journal.ensure_identity(identity)?;
    let a = journal
        .outstanding()?
        .context("journal has no outstanding attempt")?;
    let (id, state, input, operation, has_start) = (
        a.id,
        a.state,
        a.prepared.input_receipt_sha256,
        &a.prepared.operation,
        journal.start_evidence(a.id)?.is_some(),
    );
    ensure!(
        expected_attempt == id.as_i64() && state == AttemptState::Prepared,
        "cancellation requires the exact never-started Prepared attempt"
    );
    ensure!(
        !has_start,
        "Prepared attempt unexpectedly has start evidence"
    );
    let reservations = (
        journal.reserved_pids()?,
        journal.reserved_cache_ids()?,
        journal.reserved_paths()?,
    );
    create_trace(trace)?;
    durable_write(
        trace,
        "cancel-intent.json",
        &serde_json::to_vec_pretty(
            &json!({"attempt_id":expected_attempt.to_string(),"input_sha256":hex(&input),"operation_sha256":hex(&Sha256::digest(operation)),"historical_reservations_retained":true,"phone_writes":0}),
        )?,
    )?;
    journal.cancel_prepared(id)?;
    ensure!(
        reservations
            == (
                journal.reserved_pids()?,
                journal.reserved_cache_ids()?,
                journal.reserved_paths()?
            ),
        "cancellation reservation invariant changed"
    );
    let mut out = report("cancelled_prepared", Some(&a));
    out.journal_changed = true;
    durable_write(trace, "recovery.json", &serde_json::to_vec_pretty(&out)?)?;
    Ok(out)
}

/// A consuming, process-local result of the complete physical recovery checks.
/// It cannot be loaded from a receipt, cloned, or used to send an operation.
pub(crate) struct CheckedReconciliation {
    report: Report,
    trace: Option<PathBuf>,
    completion: Option<CheckedCompletion>,
}

struct CheckedCompletion {
    attempt: Box<Attempt>,
    start: StartEvidence,
    identity: Identity,
    evidence: CompletionEvidence,
    /// The inserted track's source path, recorded with its binding.
    source: Option<PathBuf>,
    control: Control,
}

impl CheckedReconciliation {
    pub(crate) fn needs_writer(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn into_report(self) -> Result<Report> {
        ensure!(
            self.completion.is_none(),
            "checked completion requires its journal transition"
        );
        Ok(self.report)
    }

    pub(crate) fn finish(self, journal: &mut Journal) -> Result<Report> {
        let Self {
            mut report,
            trace,
            completion,
        } = self;
        let completion = completion.context("recovery has no checked completion")?;
        let result = (|| {
            completion.control.check()?;
            journal.ensure_identity(&completion.identity)?;
            ensure_same_started(journal, &completion.attempt, &completion.start)?;
            completion.control.check()?;
            let track = completion.evidence.track_pid;
            journal.reconcile_completed(completion.attempt.id, completion.evidence)?;
            match &completion.source {
                Some(source) => journal.record_source(track, source),
                None => Ok(()),
            }
        })();
        match result {
            Ok(()) => {
                report.status = "reconciled_completed";
                report.journal_changed = true;
            }
            Err(error) => report.reason = Some(format!("{error:#}").chars().take(2048).collect()),
        }
        durable_write(
            trace.as_deref().context("checked recovery trace missing")?,
            "recovery.json",
            &serde_json::to_vec_pretty(&report)?,
        )?;
        Ok(report)
    }
}

fn ensure_same_started(journal: &Journal, expected: &Attempt, start: &StartEvidence) -> Result<()> {
    let actual = match dispatch(journal)? {
        Dispatch::Started(actual) => actual,
        Dispatch::Local(_) => {
            anyhow::bail!("checked recovery no longer has its original Started attempt")
        }
    };
    let left = &actual.prepared;
    let right = &expected.prepared;
    ensure!(
        actual.id == expected.id
            && actual.state == expected.state
            && left.kind == right.kind
            && left.content == right.content
            && left.new_album_pid == right.new_album_pid
            && left.new_artist_pid == right.new_artist_pid
            && left.current_media_anchor == right.current_media_anchor
            && left.operation == right.operation
            && left.input_receipt_sha256 == right.input_receipt_sha256,
        "checked recovery Started content differs from the writable journal"
    );
    let actual = journal
        .start_evidence(actual.id)?
        .context("original Ready evidence missing")?;
    ensure!(
        actual.ready_media_anchor == start.ready_media_anchor
            && actual.ready_receipt_sha256 == start.ready_receipt_sha256
            && actual.signature == start.signature,
        "checked recovery original Ready evidence differs from the writable journal"
    );
    Ok(())
}

fn verify_artwork_catalog(
    attempt: &Attempt,
    retained: &RetainedRun,
    current: &Current,
    options: &Options<'_>,
    deadline: Instant,
) -> Result<Json> {
    let art = attempt
        .prepared
        .content
        .artwork
        .as_ref()
        .context("prepared artwork missing")?;
    let cover = retained.cover.as_ref().context("retained cover missing")?;
    ensure!(
        artwork_update::prepare(
            &retained.before,
            attempt.prepared.content.track_pid,
            art.cache_id,
            cover,
            operation_time(&attempt.prepared.operation)?
        )? == attempt.prepared.operation,
        "artwork operation does not rebuild from retained cover"
    );
    let strict = |catalog: &Catalog| -> Result<Json> {
        let proof = artwork_verification::verify_independent_representative(
            &retained.before,
            catalog,
            attempt.prepared.content.track_pid,
            art.cache_id,
        )?;
        Ok(serde_json::to_value(proof)?)
    };
    // Strict-current success needs no optional saved-after artifact or wider
    // schema profile. The bridge is a separate complete preservation proof.
    let strict_refusal = match strict(&current.catalog) {
        Ok(proof) => return Ok(proof),
        Err(error) => error,
    };
    let immediate_dir = retained.step.join("after");
    let fresh_dir = options.current_state_dir;
    match fs::symlink_metadata(&immediate_dir) {
        // A run stopped before its own After capture: the fresh capture is a
        // later run's, so tracks played or skipped since are absorbed by the
        // catalogue rule, everything else compared exactly.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let proof =
                match artwork_verification::verify_independent_representative_after_listening(
                    &retained.before,
                    &current.catalog,
                    attempt.prepared.content.track_pid,
                    art.cache_id,
                    &playback::between_runs::catalogue_listening,
                ) {
                    Ok(proof) => proof,
                    Err(later) => {
                        return Err(strict_refusal
                            .context(format!("later-run projection also refused: {later:#}")));
                    }
                };
            return Ok(json!({"schema":"rustunes.artwork-recovery-later-run",
                "original_artwork_transition":proof,"verified_against":"fresh_capture_after_listening",
                "strict_fresh_projection_refusal":format!("{strict_refusal:#}").chars().take(2048).collect::<String>(),
                "fresh_permanent_readback_still_required":true,"operation_replayed":false}));
        }
        Err(e) => return Err(e.into()),
        Ok(_) => directory(&immediate_dir)?,
    }
    directory(fresh_dir)?;
    let immediate_dir = fs::canonicalize(immediate_dir)?;
    let fresh_dir = fs::canonicalize(fresh_dir)?;
    let receipt = read_bounded(&immediate_dir.join("state-receipt.json"), SMALL)?;
    let immediate = crate::engine::recovery::saved::load_saved_current(
        &crate::engine::recovery::saved::SavedCurrent {
            directory: immediate_dir.clone(),
            receipt_sha256: hex(&Sha256::digest(&receipt)),
        },
        &current.device,
        options.trace,
    )?;
    let loaded_fresh = crate::engine::recovery::saved::load_saved_current(
        &crate::engine::recovery::saved::SavedCurrent {
            directory: fresh_dir.clone(),
            receipt_sha256: hex(&current.receipt_sha256),
        },
        &current.device,
        options.trace,
    )?;
    let h = current.catalog.source_hashes();
    let actual = loaded_fresh.current.catalog.source_hashes();
    ensure!(
        h.database == actual.database
            && h.database_bytes == actual.database_bytes
            && h.wal == actual.wal
            && h.wal_bytes == actual.wal_bytes
            && current.raw_preferences == loaded_fresh.current.raw_preferences
            && current.plist_preferences == loaded_fresh.current.plist_preferences
            && current.cdb == loaded_fresh.current.cdb
            && current.artwork_db == loaded_fresh.current.artwork_db,
        "saved fresh playback evidence differs from captured Current"
    );
    let original = strict(&immediate.current.catalog)?;
    let bridge = playback::verify(
        &immediate.current,
        &immediate_dir,
        current,
        &fresh_dir,
        &options.trace.join("playback-bridge"),
        deadline.into_std(),
    )?;
    durable_write(
        options.trace,
        "retained-immediate-state.json",
        &immediate.receipt,
    )?;
    Ok(json!({"schema":"rustunes.artwork-recovery-two-stage",
        "original_artwork_transition":original,"verified_against":"matched_step_immediate_after",
        "strict_fresh_projection_refusal":format!("{strict_refusal:#}").chars().take(2048).collect::<String>(),
        "playback_bridge":bridge.evidence(),"fresh_permanent_readback_still_required":true,
        "operation_replayed":false}))
}

/// An Insert's input names the pre-session capture it was prepared from; an
/// Artwork's names none.
fn check_pre_session(input: &Json, kind: AttemptKind) -> Result<()> {
    match kind {
        AttemptKind::Insert => ordinary_preflight_number(input).map(|_| ()),
        AttemptKind::Artwork => {
            ensure!(
                input.get("pre_session_attempt").is_none(),
                "an artwork input names no pre-session capture"
            );
            Ok(())
        }
    }
}

fn ordinary_preflight_number(input: &Json) -> Result<u8> {
    ensure!(
        input["schema"] == "rustunes.insertion-input",
        "a pre-session capture belongs to an ordinary insertion"
    );
    match input["pre_session_attempt"].as_u64() {
        Some(n @ 1..=2) => Ok(n as u8),
        _ => anyhow::bail!("unsupported ordinary pre-session attempt"),
    }
}

/// The directory in `run_dir` holding a Started attempt's operation pair and
/// session transcript.
pub(crate) fn transcript_directory(run_dir: &Path, attempt: &Attempt) -> Result<PathBuf> {
    let (step, _, input) = locate_step(run_dir, attempt.prepared.input_receipt_sha256)?;
    Ok(if attempt.prepared.kind == AttemptKind::Insert {
        ordinary_preflight_directory(&step, &serde_json::from_slice(&input)?)?.join("transfer")
    } else {
        step.join("transfer")
    })
}

fn ordinary_preflight_directory(step: &Path, input: &Json) -> Result<PathBuf> {
    ensure!(
        step.file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| numbered(s, "step-"))
            .is_some(),
        "a pre-session capture requires a numbered insertion step"
    );
    let n = ordinary_preflight_number(input)?;
    let path = step.join(format!("preflight-{n:04}"));
    directory(&path)?;
    Ok(path)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArtworkPolicy {
    IndependentRepresentative,
    PreserveAlbum(u64),
}

fn validate_input_schema(input: &Json, kind: AttemptKind) -> Result<()> {
    ensure!(
        match kind {
            AttemptKind::Insert => input["schema"] == "rustunes.insertion-input",
            AttemptKind::Artwork => input["schema"] == "rustunes.artwork-input",
        },
        "unsupported retained input schema"
    );
    Ok(())
}

/// An Artwork attempt's album policy. An insertion carries no artwork.
fn artwork_policy(
    input: &Json,
    attempt: &Attempt,
    before: &Catalog,
) -> Result<Option<ArtworkPolicy>> {
    if attempt.prepared.kind != AttemptKind::Artwork {
        return Ok(None);
    }
    ensure!(
        input["schema"] == "rustunes.artwork-input"
            && input["artwork_verification"] == "exact_track_source_and_decoded_derivative",
        "unsupported retained track artwork verification policy"
    );
    let preserved = &input["preserved_album_original"];
    match input["album_policy"].as_str() {
        Some("native_representative_new_or_prior_cache") => {
            ensure!(
                preserved.is_null(),
                "independent album policy has unexpected protection proof"
            );
            Ok(Some(ArtworkPolicy::IndependentRepresentative))
        }
        Some("preserve_existing_original_bytes") => {
            let art = attempt
                .prepared
                .content
                .artwork
                .as_ref()
                .context("protected artwork missing")?;
            let target = before
                .tracks()
                .iter()
                .find(|t| t.sync_id == attempt.prepared.content.track_pid)
                .context("protected artwork target absent before update")?;
            let album = u64::from_str_radix(
                preserved["album_sync_id"]
                    .as_str()
                    .context("protected album identity absent")?,
                16,
            )?;
            let cache = *before
                .artwork()
                .album_cache_ids()
                .get(&album)
                .context("protected album registration absent")?;
            let cached = before
                .artwork()
                .cached_objects()
                .get(&(cache, 0))
                .context("protected album cached object absent")?;
            ensure!(
                album == target.album_sync_id
                    && preserved["schema"] == "rustunes.permanent-album-original"
                    && preserved["cache_id"].as_u64() == Some(u64::from(cache))
                    && preserved["media_anchor"].as_u64()
                        == Some(u64::from(before.persisted_media_anchor()))
                    && json_hash(preserved, "state_receipt_sha256")?
                        == json_hash(input, "current_state_receipt_sha256")?
                    && json_hash(preserved, "database_sha256")? == before.source_hashes().database
                    && preserved["wal_sha256"]
                        == before
                            .source_hashes()
                            .wal
                            .map(|h| json!(hex(&h)))
                            .unwrap_or(Json::Null)
                    && json_hash(preserved, "source_cover_sha256")? == art.sha256
                    && preserved["original_exact_source_bytes"] == true
                    && preserved["phone_writes"] == 0
                    && preserved["original"]["bytes"].as_u64() == Some(art.bytes)
                    && json_hash(&preserved["original"], "sha256")? == art.sha256
                    && preserved["original"]["afc_path"]
                        == format!(
                            "{}/Originals/{}",
                            crate::engine::verification::artwork::ARTWORK_ROOT,
                            cached.relative_path
                        ),
                "protected album preflight is not bound to the retained state and source cover"
            );
            Ok(Some(ArtworkPolicy::PreserveAlbum(album)))
        }
        _ => anyhow::bail!("unsupported retained album artwork policy"),
    }
}

fn ensure_identity(journal: &Journal, current: &Current) -> Result<()> {
    let library = identity::parse(&current.raw_preferences)?;
    library.ensure_matches(&identity::parse(&current.plist_preferences)?)?;
    library.ensure_matches(&current.library)?;
    let identity = Identity::new(&current.device, &library)?;
    ensure!(
        identity.device_digest() == current.identity.device_digest()
            && identity.library_digest() == current.identity.library_digest(),
        "current identity fields disagree"
    );
    journal.ensure_identity(&identity)
}

/// Opaque result of hash-bound normal-run discovery. It never grants permission
/// to send a packet or to reuse an interrupted reserved identity.
pub struct RetainedRun {
    step: PathBuf,
    before: Catalog,
    before_database: PathBuf,
    before_wal: Option<PathBuf>,
    cover: Option<Cover>,
    ready_sha256: [u8; 32],
    terminal_sha256: [u8; 32],
    /// None when the run was killed between the phone's terminal and the receipt.
    pending_sha256: Option<[u8; 32]>,
    artwork_policy: Option<ArtworkPolicy>,
    /// The absolute source path the run's retained plan names.
    source: Option<PathBuf>,
}

pub fn load_run(
    journal: &Journal,
    current: &Current,
    run_dir: &Path,
    scratch: &Path,
) -> Result<RetainedRun> {
    ensure_identity(journal, current)?;
    let attempt = journal.outstanding()?.context("no outstanding attempt")?;
    ensure!(
        attempt.state == AttemptState::ReconciliationRequired,
        "retained completion requires Started state"
    );
    let start = journal
        .start_evidence(attempt.id)?
        .context("durable start evidence missing")?;
    directory(scratch)?;
    let (step, before_dir, input) = locate_step(run_dir, attempt.prepared.input_receipt_sha256)?;
    directory(&before_dir)?;
    let catalog_dir = before_dir.join("catalog");
    directory(&catalog_dir)?;
    let input: Json = serde_json::from_slice(&input)?;
    check_pre_session(&input, attempt.prepared.kind)?;
    validate_input_schema(&input, attempt.prepared.kind)?;
    ensure!(
        json_hash(&input, "operation_sha256")?
            == <[u8; 32]>::from(Sha256::digest(&attempt.prepared.operation)),
        "retained input operation digest differs"
    );
    let state_bytes = read_bounded(&before_dir.join("state-receipt.json"), SMALL)?;
    ensure!(
        json_hash(&input, "current_state_receipt_sha256")?
            == Sha256::digest(&state_bytes).as_slice(),
        "retained before-state receipt is not bound to intent"
    );
    let state: Json = serde_json::from_slice(&state_bytes)?;
    ensure!(
        state["schema"] == "rustunes.current-state"
            && state["media_anchor"].as_u64()
                == Some(u64::from(attempt.prepared.current_media_anchor)),
        "unsupported retained state revision"
    );
    ensure!(
        json_hash(&state, "device_sha256")? == current.identity.device_digest()
            && json_hash(&state, "library_sha256")? == current.identity.library_digest(),
        "retained before-state namespace differs"
    );
    for (name, key) in [
        ("iTunesPrefs", "raw_preferences_sha256"),
        ("iTunesPrefs.plist", "plist_preferences_sha256"),
        ("iTunesCDB", "cdb_sha256"),
    ] {
        let bytes = read_bounded(
            &before_dir.join(name),
            if name == "iTunesCDB" {
                MAX_AUDIO
            } else {
                SMALL
            },
        )?;
        ensure!(
            json_hash(&state, key)? == Sha256::digest(&bytes).as_slice(),
            "retained state file digest differs"
        );
        if name != "iTunesCDB" {
            current.library.ensure_matches(&identity::parse(&bytes)?)?;
        }
    }
    let database = catalog_dir.join("MediaLibrary.sqlitedb");
    bounded_file(&database, MAX_AUDIO)?;
    let wal = catalog_dir.join("MediaLibrary.sqlitedb-wal");
    let wal = if state["wal_sha256"].is_null() {
        ensure!(!wal.try_exists()?, "unbound retained WAL exists");
        None
    } else {
        bounded_file(&wal, MAX_AUDIO)?;
        Some(wal)
    };
    let before = sqlite::read_snapshot(&database, wal.as_deref(), scratch)?;
    ensure!(
        before.source_hashes().database == json_hash(&state, "database_sha256")?
            && before.source_hashes().wal
                == if state["wal_sha256"].is_null() {
                    None
                } else {
                    Some(json_hash(&state, "wal_sha256")?)
                },
        "retained catalog hashes differ from state receipt"
    );
    ensure!(
        before.persisted_media_anchor() == attempt.prepared.current_media_anchor
            && before.database_pid() == current.catalog.database_pid()
            && before.primary_container_pid() == current.catalog.primary_container_pid(),
        "retained catalog identity or anchor differs"
    );
    let saved = music_protocol::catalogue::anchor::parse(&read_bounded(
        &before_dir.join("iTunesCDB"),
        MAX_AUDIO,
    )?)?;
    ensure!(
        saved.database_pid() == before.database_pid()
            && saved.saved_voice_memo_anchor() == templates::QUALIFIED_VOICE_MEMO_ANCHOR,
        "retained CDB profile differs from before catalog"
    );
    let transfer = if attempt.prepared.kind == AttemptKind::Insert {
        ordinary_preflight_directory(&step, &input)?.join("transfer")
    } else {
        step.join("transfer")
    };
    directory(&transfer)?;
    ensure!(
        read_bounded(
            &transfer.join("operation.plist"),
            signing::MAX_OPERATION_BYTES as u64
        )? == attempt.prepared.operation,
        "retained operation differs from journal"
    );
    ensure!(
        read_bounded(&transfer.join("operation.cig"), 21)? == start.signature,
        "retained signature differs from Started evidence"
    );
    // A run killed after the phone's SyncFinished but before this receipt was
    // written leaves none. The transcript, which must then end in that
    // SyncFinished, gives the terminal; the host-side hashes the receipt
    // would hold are read back fresh below (permanent audio, cover).
    let pending_path = transfer.join("pending-verification.plist");
    let pending_bytes = match fs::symlink_metadata(&pending_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        _ => Some(read_bounded(&pending_path, SMALL)?),
    };
    let pending = pending_bytes
        .as_deref()
        .map(plist::from_bytes::<Value>)
        .transpose()?;
    let pending = pending.as_ref().map(dict).transpose()?;
    let terminal = pending
        .map(|pending| pending_terminal(pending, &attempt, &start))
        .transpose()?;
    let cover = if let Some(art) = &attempt.prepared.content.artwork {
        if let Some(pending) = pending {
            ensure!(
                uint(pending, "TransferredAudioBytes")?
                    == attempt.prepared.content.source.file_bytes
                    && uint(pending, "CacheID")? == u64::from(art.cache_id)
                    && uint(pending, "CoverBytes")? == art.bytes
                    && plist_hash(pending, "StagedCoverSHA256")? == art.sha256,
                "retained artwork completion differs"
            );
        }
        let bytes = read_bounded(&transfer.join("source-cover.jpg"), 32 * SMALL)?;
        ensure!(
            bytes.len() as u64 == art.bytes
                && <[u8; 32]>::from(Sha256::digest(&bytes)) == art.sha256,
            "retained cover differs from reserved JPEG"
        );
        Some(Cover {
            checksum: u32::try_from(bytes.len())?
                .checked_add(17)
                .context("cover checksum overflow")?,
            sha256: art.sha256,
            bytes,
        })
    } else {
        if let Some(pending) = pending {
            ensure!(
                uint(pending, "AudioBytes")? == attempt.prepared.content.source.file_bytes
                    && plist_hash(pending, "EncodedAudioSHA256")?
                        == attempt.prepared.content.source.audio.sha256,
                "retained audio completion differs"
            );
        }
        None
    };
    let artwork_policy = artwork_policy(&input, &attempt, &before)?;
    let terminal = verify_transcript(&transfer, &attempt, &start, terminal, &before, current)?;
    Ok(RetainedRun {
        step,
        before,
        before_database: database,
        before_wal: wal,
        cover,
        ready_sha256: start.ready_receipt_sha256,
        terminal_sha256: terminal,
        pending_sha256: pending_bytes.map(|bytes| Sha256::digest(&bytes).into()),
        artwork_policy,
        source: planned_source(run_dir, &input)?,
    })
}

/// The absolute path of the source the run's retained plan (bound by the
/// input's plan digest) gave the insertion, as the insertion itself records
/// it with the binding. None when the run no longer holds that plan.
fn planned_source(run_dir: &Path, input: &Json) -> Result<Option<PathBuf>> {
    let path = run_dir.join("plan.json");
    if input.get("plan_sha256").is_none() || !path.try_exists()? {
        return Ok(None);
    }
    let bytes = read_bounded(&path, MAX_PLAN)?;
    if Sha256::digest(&bytes).as_slice() != json_hash(input, "plan_sha256")? {
        return Ok(None);
    }
    let plan: Json = serde_json::from_slice(&bytes)?;
    let root = Path::new(
        plan["source_root"]
            .as_str()
            .context("plan source root absent")?,
    );
    let relative = input["source"]
        .as_str()
        .context("insertion source absent")?;
    ensure!(root.is_absolute(), "plan source root is relative");
    Ok(Some(root.join(relative)))
}

/// Check a run's pending-verification receipt against the attempt and return
/// the terminal frame digest it recorded.
fn pending_terminal(
    pending: &Dictionary,
    attempt: &Attempt,
    start: &StartEvidence,
) -> Result<[u8; 32]> {
    ensure!(
        text(pending, "Status")? == "PendingIndependentCatalogVerification",
        "unsupported completion evidence"
    );
    ensure!(
        pid(pending, "TrackPID")? == attempt.prepared.content.track_pid
            && text(pending, "PermanentPath")? == attempt.prepared.content.path
            && uint(pending, "MediaAnchor")?
                == u64::from(attempt.prepared.current_media_anchor) + 1,
        "retained completion target differs"
    );
    ensure!(
        plist_hash(pending, "OperationSHA256")?
            == Sha256::digest(&attempt.prepared.operation).as_slice()
            && plist_hash(pending, "ReadyFrameSHA256")? == start.ready_receipt_sha256
            && plist_hash(pending, "PermanentAudioSHA256")?
                == attempt.prepared.content.source.full_file_sha256,
        "retained completion hashes differ"
    );
    plist_hash(pending, "TerminalFrameSHA256")
}

/// Only fixed immediate children of a normal run are visited. Descriptor paths
/// are never followed; their presence only identifies the supported wrapper.
pub(crate) fn locate_step(
    run_dir: &Path,
    input_hash: [u8; 32],
) -> Result<(PathBuf, PathBuf, Vec<u8>)> {
    directory(run_dir)?;
    let mut roots = vec![run_dir.to_path_buf()];
    if run_dir.join("run.json").try_exists()? {
        let descriptor: Json =
            serde_json::from_slice(&read_bounded(&run_dir.join("run.json"), SMALL)?)?;
        ensure!(
            descriptor["schema"] == "rustunes.run",
            "unsupported outer run descriptor"
        );
        if run_dir.join("audio").try_exists()? {
            directory(&run_dir.join("audio"))?;
            roots.push(run_dir.join("audio"));
        }
        for path in bounded_entries(run_dir, 8192)? {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| numbered(n, "cover-").is_some())
            {
                directory(&path)?;
                roots.push(path);
            }
        }
    }
    let mut candidates = Vec::new();
    let mut entry_count = 0usize;
    for root in roots {
        if root.join("input.json").try_exists()? {
            candidates.push((root.clone(), root.join("before")));
        }
        let entries = bounded_entries(&root, 8192)?;
        entry_count = entry_count
            .checked_add(entries.len())
            .context("retained entry count overflow")?;
        ensure!(
            entry_count <= 25000,
            "retained run entry aggregate exceeds bound"
        );
        for entry in entries {
            let Some(number) = entry
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| numbered(s, "step-"))
            else {
                continue;
            };
            directory(&entry)?;
            if entry.join("input.json").try_exists()? {
                candidates.push((
                    entry,
                    if number == 1 {
                        root.join("before")
                    } else {
                        root.join(format!("step-{:04}/after", number - 1))
                    },
                ));
            }
            ensure!(
                candidates.len() <= 4096,
                "retained candidate count exceeds bound"
            );
        }
    }
    let mut selected = None;
    let mut total = 0u64;
    for (step, before) in candidates {
        let input = read_bounded(&step.join("input.json"), SMALL)?;
        total = total
            .checked_add(input.len() as u64)
            .context("retained input size overflow")?;
        ensure!(
            total <= 16 * SMALL,
            "retained input aggregate exceeds bound"
        );
        if <[u8; 32]>::from(Sha256::digest(&input)) == input_hash {
            ensure!(
                selected.is_none(),
                "multiple retained steps match the outstanding intent"
            );
            // This locator also serves byte-only discovery tests. Full input
            // parsing remains mandatory in each consumer before any device I/O.
            let parsed: Json = serde_json::from_slice(&input).unwrap_or(Json::Null);
            let before = if parsed.get("pre_session_attempt").is_some() {
                let phase = ordinary_preflight_directory(&step, &parsed)?;
                ensure!(
                    read_bounded(&phase.join("input.json"), SMALL)? == input,
                    "published insertion input differs from retained pre-session input"
                );
                phase.join("before")
            } else {
                before
            };
            selected = Some((step, before, input));
        }
    }
    selected.context("no retained step matches immutable journal input digest")
}
fn numbered(name: &str, prefix: &str) -> Option<usize> {
    let n = name.strip_prefix(prefix)?;
    if n.len() != 4 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok().filter(|n| (1..=4096).contains(n))
}

/// Check the retained session and return its terminal frame's digest, which
/// must equal `terminal` when the run's receipt recorded one.
pub(crate) fn verify_transcript(
    directory_path: &Path,
    attempt: &Attempt,
    start: &StartEvidence,
    terminal: Option<[u8; 32]>,
    before: &Catalog,
    current: &Current,
) -> Result<[u8; 32]> {
    let mut frames = BTreeMap::new();
    let mut total = 0u64;
    for path in bounded_entries(directory_path, 25000)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        ensure!(
            !name.ends_with("-partial.atc"),
            "retained partial frame leaves session outcome uncertain"
        );
        let Some((sequence, suffix)) = name.split_once('-') else {
            continue;
        };
        let outgoing = match suffix {
            "outgoing.atc" => true,
            "received.atc" => false,
            _ => continue,
        };
        ensure!(
            (3..=4).contains(&sequence.len()) && sequence.bytes().all(|b| b.is_ascii_digit()),
            "unsupported frame sequence name"
        );
        let sequence: usize = sequence.parse()?;
        ensure!(sequence < 1024, "retained frame count exceeds bound");
        let bytes = read_bounded(&path, FRAME_CAP)?;
        total = total
            .checked_add(bytes.len() as u64)
            .context("frame byte overflow")?;
        ensure!(total <= FRAME_CAP, "retained session bytes exceed bound");
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let value = frame(&bytes)?;
        ensure!(
            frames.insert(sequence, (outgoing, hash, value)).is_none(),
            "ambiguous retained frame sequence"
        );
    }
    ensure!(
        !frames.is_empty() && frames.keys().copied().eq(0..frames.len()),
        "retained session frames are missing"
    );
    let own = (attempt.prepared.content.track_pid as i64).to_string();
    let eligible = EligibleSnapshot::new(before, &current.library);
    let mut request = false;
    let mut ready = false;
    let mut finished = false;
    let mut complete = false;
    let mut ended = None;
    let mut ancillary = None::<BTreeSet<String>>;
    let mut declined = BTreeSet::new();
    let mut templates = None;
    for (number, (outgoing, hash, value)) in &frames {
        ensure!(ended.is_none(), "frames follow retained terminal");
        let root = dict(value)?;
        let command = text(root, "Command")?;
        if *outgoing {
            if command == "Ping" {
                ensure!(
                    *number == 0 && root.len() == 2 && uint(root, "Session")? == 1,
                    "unsupported retained Ping"
                );
                continue;
            }
            ensure!(
                uint(root, "Session")? == 2,
                "retained outgoing session differs"
            );
            match command {
                "RequestingSync" => {
                    ensure!(!request && !ready, "duplicate retained request");
                    let host = child(child(root, "Params")?, "HostInfo")?;
                    let built = templates::build(
                        &current.library,
                        text(host, "SyncHostName")?,
                        attempt.prepared.current_media_anchor,
                        1,
                    )?;
                    ensure!(
                        built.request == *value,
                        "retained request differs from qualified runtime profile"
                    );
                    templates = Some(built);
                    request = true;
                }
                "FinishedSyncingMetadata" => {
                    ensure!(
                        ready && !finished && !complete,
                        "metadata completion order differs"
                    );
                    let mut expected = templates
                        .as_ref()
                        .context("retained request absent")?
                        .finished
                        .clone();
                    expected
                        .as_dictionary_mut()
                        .unwrap()
                        .get_mut("Params")
                        .unwrap()
                        .as_dictionary_mut()
                        .unwrap()
                        .get_mut("DataclassAnchors")
                        .unwrap()
                        .as_dictionary_mut()
                        .unwrap()
                        .insert(
                            "Media".into(),
                            (attempt.prepared.current_media_anchor + 1)
                                .to_string()
                                .into(),
                        );
                    ensure!(expected == *value, "retained metadata completion differs");
                    finished = true;
                }
                "FileComplete" => {
                    ensure!(
                        finished && ancillary.is_some() && !complete && root.len() == 3,
                        "own completion order differs"
                    );
                    let params = child(root, "Params")?;
                    ensure!(
                        params.len() == 3
                            && text(params, "AssetID")? == own
                            && text(params, "Dataclass")? == "Media"
                            && text(params, "AssetPath")?
                                == match attempt.prepared.kind {
                                    AttemptKind::Insert => attempt.prepared.content.path.as_str(),
                                    AttemptKind::Artwork => "",
                                },
                        "retained FileComplete targets another asset"
                    );
                    complete = true;
                }
                "FileError" => {
                    ensure!(complete && root.len() == 3, "ancillary error order differs");
                    let params = child(root, "Params")?;
                    let id = text(params, "AssetID")?;
                    ensure!(
                        params.len() == 4
                            && text(params, "Dataclass")? == "Media"
                            && params.get("ErrorCode").and_then(Value::as_signed_integer)
                                == Some(16)
                            && params.get("ErrorIsVisible").and_then(Value::as_boolean)
                                == Some(false)
                            && ancillary.as_ref().is_some_and(|set| set.contains(id))
                            && declined.insert(id.to_owned()),
                        "unsupported retained asset error"
                    );
                }
                _ => anyhow::bail!("unsupported retained outgoing command"),
            }
        } else {
            ensure!(
                uint(root, "Type")? == 0
                    && root
                        .get("Id")
                        .and_then(Value::as_unsigned_integer)
                        .is_some()
                    && root.len() == if root.contains_key("Params") { 5 } else { 4 },
                "unsupported retained incoming envelope"
            );
            if ready {
                ensure!(
                    uint(root, "Session")? == 2,
                    "retained post-Ready session differs"
                );
            }
            match command {
                "ReadyForSync" => {
                    ensure!(
                        request
                            && !ready
                            && *hash == start.ready_receipt_sha256
                            && uint(root, "Session")? == 2,
                        "retained Ready is not journal-bound"
                    );
                    let params = child(root, "Params")?;
                    let anchors = child(params, "DataclassAnchors")?;
                    ensure!(
                        text(anchors, "Media")?
                            == attempt.prepared.current_media_anchor.to_string(),
                        "retained Ready anchor differs"
                    );
                    let reply = child(params, "DeviceInfo")?
                        .get("Grappa")
                        .and_then(Value::as_data)
                        .context("retained Grappa missing")?;
                    ensure!(
                        signing::FixedHostSession::from_reply(&signing::HOST_REQUEST, reply)?
                            .sign(&attempt.prepared.operation)?
                            == start.signature,
                        "stored signature does not verify against retained Ready and operation"
                    );
                    ready = true;
                }
                "AssetManifest" => {
                    ensure!(
                        finished && !complete && ancillary.is_none(),
                        "unexpected retained manifest"
                    );
                    let declined = match attempt.prepared.kind {
                        AttemptKind::Insert => eligible.audio(&own)?.validate_manifest(value, 2)?,
                        AttemptKind::Artwork => {
                            eligible.artwork(&own)?.validate_manifest(value, 2)?
                        }
                    };
                    ancillary = Some(declined.into_iter().collect());
                }
                "SyncFinished" => {
                    if root.contains_key("Params") {
                        child(root, "Params")?;
                    }
                    ensure!(
                        complete
                            && terminal.is_none_or(|t| *hash == t)
                            && ancillary.as_ref() == Some(&declined),
                        "retained terminal lacks exact completed asset evidence"
                    );
                    ended = Some(*hash);
                }
                "Pong" | "Capabilities" => ensure!(!ready, "unexpected late handshake message"),
                "SyncAllowed" | "InstalledAssets" | "AssetMetrics" | "Progress" => {
                    music_protocol::protocol::session::check_telemetry(root)?
                }
                _ => anyhow::bail!("retained fault or unsupported incoming command"),
            }
        }
    }
    ended.context("retained successful terminal missing")
}

fn insert_groups(operation: &[u8]) -> Result<(u64, u64)> {
    let value: Value = plist::from_bytes(operation)?;
    let ops = dict(&value)?
        .get("operations")
        .and_then(Value::as_array)
        .context("operation list missing")?;
    ensure!(ops.len() == 2, "unsupported insertion operation count");
    let item = child(dict(&ops[1])?, "item")?;
    Ok((pid(item, "album_pid")?, pid(item, "artist_pid")?))
}
fn operation_time(operation: &[u8]) -> Result<SystemTime> {
    let value: Value = plist::from_bytes(operation)?;
    Ok(dict(&value)?
        .get("timestamp")
        .and_then(Value::as_date)
        .context("operation timestamp missing")?
        .into())
}
fn dict(v: &Value) -> Result<&Dictionary> {
    v.as_dictionary().context("expected plist dictionary")
}
fn child<'a>(d: &'a Dictionary, key: &str) -> Result<&'a Dictionary> {
    dict(d.get(key).context("missing plist dictionary")?)
}
fn text<'a>(d: &'a Dictionary, key: &str) -> Result<&'a str> {
    d.get(key)
        .and_then(Value::as_string)
        .context("missing plist string")
}
fn uint(d: &Dictionary, key: &str) -> Result<u64> {
    d.get(key)
        .and_then(Value::as_unsigned_integer)
        .context("missing plist integer")
}
fn pid(d: &Dictionary, key: &str) -> Result<u64> {
    Ok(d.get(key)
        .and_then(Value::as_signed_integer)
        .context("missing signed identity")? as u64)
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid lowercase SHA256"
    );
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}
fn json_hash(value: &Json, key: &str) -> Result<[u8; 32]> {
    decode_hash(
        value
            .get(key)
            .and_then(Json::as_str)
            .context("missing JSON digest")?,
    )
}
fn plist_hash(value: &Dictionary, key: &str) -> Result<[u8; 32]> {
    decode_hash(text(value, key)?)
}
/// One retained ATC frame: its length prefix, then the plist.
pub(crate) fn frame(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() >= 4 && u32::from_le_bytes(bytes[..4].try_into()?) as usize == bytes.len() - 4,
        "retained ATC frame length differs"
    );
    Ok(plist::from_bytes(&bytes[4..])?)
}
fn directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "retained evidence directory is not regular"
    );
    Ok(())
}
fn bounded_entries(path: &Path, maximum: usize) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(path)? {
        ensure!(
            out.len() < maximum,
            "retained directory entry bound exceeded"
        );
        out.push(entry?.path());
    }
    Ok(out)
}
fn bounded_file(path: &Path, cap: u64) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.len() <= cap,
        "retained artifact type or size exceeds bound"
    );
    Ok(meta)
}
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let before = bounded_file(path, cap)?;
    let mut file = File::open(path)?;
    let held = file.metadata()?;
    ensure!(
        before.dev() == held.dev() && before.ino() == held.ino(),
        "retained artifact changed before open"
    );
    let mut bytes = Vec::new();
    file.by_ref().take(cap + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "retained artifact changed size"
    );
    for after in [file.metadata()?, fs::symlink_metadata(path)?] {
        ensure!(
            after.is_file()
                && before.dev() == after.dev()
                && before.ino() == after.ino()
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "retained artifact changed during read"
        );
    }
    Ok(bytes)
}
