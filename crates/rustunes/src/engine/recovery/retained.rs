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
fn numbered(name: &str, prefix: &str) -> Option<usize> {
    let n = name.strip_prefix(prefix)?;
    if n.len() != 4 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok().filter(|n| (1..=4096).contains(n))
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
