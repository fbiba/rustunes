//! Read-only controller: fresh prefs/catalogue and one complete audio pass.
//! Durable progress is incomplete evidence until complete-index.json is present.
use music_protocol::protocol::afc::copy;
use music_protocol::protocol::trace::private_write;
use music_protocol::protocol::{
    afc::stat::{self as afc_stat, Attributes, regular_size, unchanged},
    trace::private_file,
};

use crate::engine::execution::device::DeviceProvider;
use anyhow::{Context, Result, ensure};
use idevice::afc::AfcClient;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, File},
    future::Future,
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::Path,
    sync::{OnceLock, atomic::AtomicBool},
    time::{Duration, SystemTime},
};
use tokio::time::{Instant, timeout_at};

use crate::{
    engine::state::audio_index::{
        self, Absent, CatalogBinding, IndexIdentity, IndexProgress, OtherAudio, ReadControl,
        TrackAudio, fingerprint_json,
    },
    engine::state::journal::Identity,
};
use music_protocol::catalogue::identity;
use music_protocol::catalogue::sqlite::Catalog;

const PREFS: [&str; 2] = ["iTunesPrefs", "iTunesPrefs.plist"];
const PREFS_CAP: u64 = 1024 * 1024;

/// Unbound capture for the read-only `snapshot` command: the whole catalogue
/// and its audio, with no expected identity or track count.
pub async fn capture<F, Fut>(trace: &Path, maximum: Duration, connect: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    capture_scoped(trace, maximum, None, &AtomicBool::new(false), None, connect).await
}
/// Initial unbound index capture under a caller's unchanged absolute deadline.
/// The caller validates its requested duration before constructing this control;
/// a partial remaining second is still a valid bounded capture window.
pub(crate) async fn capture_with_control<F, Fut>(
    trace: &Path,
    control: &crate::engine::execution::native_insert::Control,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    ensure!(
        control.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(1800),
        "capture caller deadline exceeds 1800 seconds"
    );
    capture_scoped_until(
        trace,
        control.deadline,
        None,
        &control.cancelled,
        None,
        connect,
    )
    .await
}
/// `capture_with_control` allowing rows without audio (`Absent`): rows an
/// interrupted insertion left (its own targets), and, when allowed, rows
/// whose file is gone, are recorded in the receipt instead of refusing the
/// catalogue. Only `snapshot::open_allowing` with the same allowance opens
/// the result.
pub(crate) async fn capture_allowing<F, Fut>(
    trace: &Path,
    absent: Absent<'_>,
    control: &crate::engine::execution::native_insert::Control,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    control.check()?;
    ensure!(
        control.deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(1800),
        "capture caller deadline exceeds 1800 seconds"
    );
    capture_scoped_allowing_until(
        trace,
        control.deadline,
        (None, absent),
        &control.cancelled,
        None,
        connect,
    )
    .await
}
fn ensure_expected_identity(
    expected: Option<&IndexIdentity>,
    observed: &IndexIdentity,
) -> Result<()> {
    ensure!(
        expected.is_none_or(|expected| expected == observed),
        "fresh preferences/device identity differs before catalogue/audio capture"
    );
    Ok(())
}

fn bind_capture_identity(
    expected: Option<&IndexIdentity>,
    session: &OnceLock<IndexIdentity>,
    observed: &IndexIdentity,
) -> Result<()> {
    ensure_expected_identity(expected, observed)?;
    ensure_expected_identity(Some(session.get_or_init(|| observed.clone())), observed)
}

async fn capture_scoped<F, Fut>(
    trace: &Path,
    maximum: Duration,
    expected_tracks: Option<usize>,
    cancelled: &AtomicBool,
    expected_identity: Option<&IndexIdentity>,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    ensure!(
        (1..=1800).contains(&maximum.as_secs()),
        "capture deadline must be 1..1800 seconds"
    );
    capture_scoped_until(
        trace,
        Instant::now() + maximum,
        expected_tracks,
        cancelled,
        expected_identity,
        connect,
    )
    .await
}
async fn capture_scoped_until<F, Fut>(
    trace: &Path,
    deadline: Instant,
    expected_tracks: Option<usize>,
    cancelled: &AtomicBool,
    expected_identity: Option<&IndexIdentity>,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    capture_scoped_allowing_until(
        trace,
        deadline,
        (expected_tracks, Absent::NONE),
        cancelled,
        expected_identity,
        connect,
    )
    .await
}
async fn capture_scoped_allowing_until<F, Fut>(
    trace: &Path,
    deadline: Instant,
    (expected_tracks, absent): (Option<usize>, Absent<'_>),
    cancelled: &AtomicBool,
    expected_identity: Option<&IndexIdentity>,
    connect: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DeviceProvider>>,
{
    check_capture_control(deadline, cancelled)?;
    DirBuilder::new()
        .mode(0o700)
        .create(trace)
        .context("new private capture directory required")?;
    let mut retry_attempted = false;
    let work = async {
        check_capture_control(deadline, cancelled)?;
        let provider = timeout_at(
            deadline.min(Instant::now() + Duration::from_secs(15)),
            connect(),
        )
        .await
        .context("USB discovery deadline")??;
        let session_identity = OnceLock::new();
        let control = ReadControl {
            deadline,
            cancelled,
        };
        capture_attempts(
            trace,
            deadline,
            cancelled,
            &mut retry_attempted,
            |progress| {
                capture_inner(
                    trace,
                    (expected_tracks, absent),
                    &control,
                    progress,
                    expected_identity,
                    &session_identity,
                    &provider,
                )
            },
        )
        .await
    };
    let result = capture_work(work, deadline, cancelled).await;
    let outcome = match &result {
        Ok(()) => {
            json!({"schema":"rustunes.device-index-outcome","complete":true,"retry_attempted":retry_attempted,"deadline_renewed":false})
        }
        Err(error) => {
            json!({"schema":"rustunes.device-index-outcome","complete":false,"error":format!("{error:#}"),"partial_evidence_only":true,"retry_attempted":retry_attempted,"deadline_renewed":false})
        }
    };
    write_json(&trace.join("outcome.json"), &outcome)?;
    // A plain complete snapshot may stand in for the next step's before-state
    // (removal_batch::capture checks it against a fresh state capture).
    if result.is_ok()
        && expected_tracks.is_none()
        && expected_identity.is_none()
        && let Ok(path) = trace.canonicalize()
    {
        crate::engine::execution::device::remember_snapshot(&path);
    }
    result
}

fn check_capture_control(deadline: Instant, cancelled: &AtomicBool) -> Result<()> {
    ensure!(
        !cancelled.load(std::sync::atomic::Ordering::Relaxed),
        "capture cancelled; partial evidence only"
    );
    ensure!(
        Instant::now() < deadline,
        "whole read-only capture deadline expired"
    );
    Ok(())
}

async fn capture_work<F>(work: F, deadline: Instant, cancelled: &AtomicBool) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    check_capture_control(deadline, cancelled)?;
    let cancellation = async {
        while !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    let result = tokio::select! {
        result = timeout_at(deadline, work) => result.context("whole read-only capture original deadline expired")?,
        () = cancellation => anyhow::bail!("capture cancelled; partial evidence only"),
    };
    check_capture_control(deadline, cancelled)?;
    result
}

async fn capture_attempts<F, Fut>(
    trace: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    retry_attempted: &mut bool,
    mut attempt: F,
) -> Result<()>
where
    F: FnMut(File) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for number in 1..=2 {
        check_capture_control(deadline, cancelled)?;
        let progress = private_file(&trace.join("progress.jsonl"))?;
        progress.sync_all()?;
        *retry_attempted = number == 2;
        let result = capture_work(attempt(progress), deadline, cancelled).await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                let initial = error
                    .downcast_ref::<audio_index::InitialCatalogGuardDrift>()
                    .is_some()
                    || error
                        .downcast_ref::<crate::engine::verification::catalogue_capture::CatalogCaptureDrift>()
                        .is_some();
                let late = error
                    .downcast_ref::<audio_index::LateCatalogGuardDrift>()
                    .is_some();
                if number == 2 || !(initial || late) {
                    return Err(error);
                }
                // Initial failures precede audio reads. Retention still
                // requires the exact initial artifact set, and empty progress
                // unless a pass completed before the drift.
                retain_failed_attempt(trace, &error, late)?;
                check_capture_control(deadline, cancelled)?;
                crate::engine::observe(crate::engine::Event::Observation(if late {
                    crate::engine::Observation::RecapturingAfterAudioIndex
                } else {
                    crate::engine::Observation::RecapturingBeforeAudioIndex
                }))?;
            }
        }
    }
    unreachable!("two-attempt capture always returns")
}

fn retain_failed_attempt(trace: &Path, error: &anyhow::Error, late: bool) -> Result<()> {
    let names = [
        "before-iTunesPrefs",
        "before-iTunesPrefs-receipt.json",
        "before-iTunesPrefs.plist",
        "before-iTunesPrefs.plist-receipt.json",
        "catalog",
        "progress.jsonl",
    ];
    let actual = fs::read_dir(trace)?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    ensure!(
        actual == names.iter().map(std::ffi::OsString::from).collect(),
        "initial drift has unexpected capture artifacts; no retry"
    );
    ensure!(
        late || fs::metadata(trace.join("progress.jsonl"))?.len() == 0,
        "initial drift has audio progress; no retry"
    );
    let failed = trace.join("failed-attempt-1");
    DirBuilder::new().mode(0o700).create(&failed)?;
    for name in names {
        fs::rename(trace.join(name), failed.join(name))?;
    }
    write_json(
        &failed.join("outcome.json"),
        &json!({
            "schema":"rustunes.device-index-outcome", "complete":false,
            "error":format!("{error:#}"),
            "initial_catalog_guard_drift":error.downcast_ref::<audio_index::InitialCatalogGuardDrift>().is_some(),
            "late_catalog_guard_drift":late,
            "catalog_capture_drift_files":error.downcast_ref::<crate::engine::verification::catalogue_capture::CatalogCaptureDrift>().map(|drift| drift.changed_files()),
            "partial_evidence_only":true, "retry_attempted":false,
            "deadline_renewed":false, "phone_writes":0,
        }),
    )?;
    File::open(&failed)?.sync_all()?;
    File::open(trace)?.sync_all()?;
    Ok(())
}

async fn capture_inner(
    trace: &Path,
    selection: (Option<usize>, Absent<'_>),
    control: &ReadControl<'_>,
    mut progress: File,
    expected_identity: Option<&IndexIdentity>,
    session_identity: &OnceLock<IndexIdentity>,
    provider: &DeviceProvider,
) -> Result<()> {
    let (expected_tracks, absent) = selection;
    let ReadControl {
        deadline,
        cancelled,
    } = *control;
    check_capture_control(deadline, cancelled)?;
    let library = prefs(provider, trace, "before", deadline).await?;
    let identity = Identity::new(&provider.udid, &library)?;
    let identity = identity.index_identity();
    bind_capture_identity(expected_identity, session_identity, &identity)?;
    let catalog = crate::engine::verification::catalogue_capture::read_until(
        Some(&trace.join("catalog")),
        deadline,
        || async { Ok(provider.clone()) },
    )
    .await?;
    if let Some(expected) = expected_tracks {
        ensure!(
            catalog.tracks().len() == expected,
            "fresh catalog track count differs from reviewed scope"
        );
    }
    let required = audio_index::required_bytes_allowing(&catalog, &absent.rows(&catalog))?;
    crate::engine::observe(crate::engine::Event::Observation(
        crate::engine::Observation::FreshCatalogue {
            tracks: catalog.tracks().len(),
            bytes: required,
        },
    ))?;
    let afc = timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(20)),
        music_protocol::protocol::afc::connect_checked(provider),
    )
    .await
    .context("audio AFC connection deadline")??;
    let mut digest = Sha256::new();
    let mut completed = 0usize;
    let mut completed_bytes = 0u64;
    let mut total = catalog.tracks().len();
    let mut reused_track = None;
    let mut copying = None;
    let mut report = |event| {
        let record = match event {
            IndexProgress::Started {
                track_count,
                total_bytes,
                identity,
                catalog,
            } => {
                total = track_count;
                Some(
                    json!({"event":"started","track_count":track_count,"total_bytes":total_bytes,
                    "identity":identity_json(&identity),"catalog":catalog_json(&catalog),"unix_time_ns":unix_time_ns()?}),
                )
            }
            IndexProgress::FileStarted {
                sync_id,
                afc_path,
                file_bytes,
            } => {
                // A cached fingerprint may yet stand in for the copy.
                copying = Some((sync_id, file_bytes));
                Some(
                    json!({"event":"file_started","sync_id":format!("{sync_id:016X}"),"afc_path":afc_path,"file_bytes":file_bytes}),
                )
            }
            IndexProgress::FileBytes {
                sync_id,
                file_bytes_read,
                ..
            } => {
                if let Some((copied, file_bytes)) = copying.filter(|(id, _)| *id == sync_id) {
                    observe_copy(&catalog, copied, file_bytes_read, file_bytes)?;
                }
                None
            }
            IndexProgress::Reused { sync_id } => {
                reused_track = Some(sync_id);
                Some(json!({"event":"fingerprint_reused","sync_id":format!("{sync_id:016X}")}))
            }
            IndexProgress::Fingerprinted(row) => {
                completed += 1;
                completed_bytes += row.fingerprint.file_bytes;
                crate::engine::observe(crate::engine::Event::Observation(
                    crate::engine::Observation::AudioFingerprinted {
                        track: row.sync_id,
                        completed,
                        total,
                        bytes: completed_bytes,
                        other_codec: false,
                        reused: reused_track.take() == Some(row.sync_id),
                    },
                ))?;
                Some(
                    json!({"event":"fingerprinted","ordinal":completed,"record":track_json(&row),"unix_time_ns":unix_time_ns()?}),
                )
            }
            IndexProgress::OtherFingerprinted(row) => {
                completed += 1;
                completed_bytes += row.file_bytes;
                crate::engine::observe(crate::engine::Event::Observation(
                    crate::engine::Observation::AudioFingerprinted {
                        track: row.sync_id,
                        completed,
                        total,
                        bytes: completed_bytes,
                        other_codec: true,
                        reused: reused_track.take() == Some(row.sync_id),
                    },
                ))?;
                Some(
                    json!({"event":"other_fingerprinted","ordinal":completed,"record":other_json(&row),"unix_time_ns":unix_time_ns()?}),
                )
            }
        };
        if let Some(record) = record {
            append_progress(&mut progress, &mut digest, &record)?;
        }
        Ok(())
    };
    let read_control = ReadControl {
        deadline,
        cancelled,
    };
    let index = audio_index::read_all_allowing(
        afc,
        &catalog,
        absent,
        identity.clone(),
        trace,
        &read_control,
        &mut report,
    )
    .await?;
    index.ensure_matches(&catalog, &identity)?;
    let after = prefs(provider, trace, "after", deadline).await?;
    library.ensure_matches(&after)?;
    let mut body = json!({"schema":"rustunes.device-audio-index","complete":true,
        "identity":identity_json(index.identity()),"catalog":catalog_json(index.catalog()),
        "track_count":index.tracks().len()+index.other_audio().len(),"alac_track_count":index.tracks().len(),"other_track_count":index.other_audio().len(),"total_bytes":completed_bytes,"encoded_audio_kind":"encoded-alac-not-pcm",
        "records":index.tracks().values().map(track_json).collect::<Vec<_>>(),
        "other_records":index.other_audio().values().map(other_json).collect::<Vec<_>>(),
        "progress_sha256":hex(&digest.finalize()),"completed_unix_time_ns":unix_time_ns()?,
        "fresh_prefs_before_after_components_equal":true,"atomic_snapshot_claim":false,
        "catalog_metadata":catalog_metadata(&catalog)});
    if !index.unfilled().is_empty() {
        body["unfilled_records"] = json!(
            index
                .unfilled()
                .iter()
                .map(|id| format!("{id:016X}"))
                .collect::<Vec<_>>()
        );
    }
    if !index.gone().is_empty() {
        body["gone_records"] = json!(
            index
                .gone()
                .iter()
                .map(|id| format!("{id:016X}"))
                .collect::<Vec<_>>()
        );
    }
    progress.sync_all()?;
    publish_complete_index(trace, &body, deadline, cancelled)?;
    crate::engine::observe(crate::engine::Event::Observation(
        crate::engine::Observation::AudioIndexRetained {
            tracks: completed,
            bytes: completed_bytes,
        },
    ))?;
    Ok(())
}

/// Report `read` bytes of track `sync_id`'s audio copied for its fingerprint.
fn observe_copy(catalog: &Catalog, sync_id: u64, read: u64, file_bytes: u64) -> Result<()> {
    let track = catalog.tracks().iter().find(|t| t.sync_id == sync_id);
    crate::engine::observe(crate::engine::Event::Observation(
        crate::engine::Observation::CopyingTrackAudio {
            track: sync_id,
            title: track.map_or("", |t| &t.metadata.title),
            artist: track.map_or("", |t| &t.metadata.artist),
            read,
            file_bytes,
        },
    ))
}

async fn prefs(
    provider: &DeviceProvider,
    trace: &Path,
    phase: &str,
    deadline: Instant,
) -> Result<identity::LibraryIdentity> {
    let mut afc = timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(20)),
        music_protocol::protocol::afc::connect_checked(provider),
    )
    .await
    .context("prefs AFC connection deadline")??;
    let mut copies = Vec::new();
    for name in PREFS {
        let path = format!("iTunes_Control/iTunes/{name}");
        let before = attributes(&mut afc, &path, deadline).await?;
        let size = regular_size(&before)?;
        ensure!((1..=PREFS_CAP).contains(&size), "prefs size exceeds bound");
        let mut bytes = Vec::new();
        let (copied, closed) = copy::read_file(
            &mut afc,
            &path,
            &mut bytes,
            size,
            copy::CopyLimits {
                maximum_size: PREFS_CAP,
                maximum_reads: 256,
            },
            deadline,
            None,
            "open prefs read-only",
        )
        .await?;
        copied.map_err(|failure| failure.error)?;
        closed
            .context("prefs close not attempted")?
            .map_err(|failure| failure.error)?;
        ensure!(bytes.len() as u64 == size, "prefs copy length differs");
        let after = attributes(&mut afc, &path, deadline).await?;
        ensure!(unchanged(&before, &after), "prefs changed during read");
        private_write(&trace.join(format!("{phase}-{name}")), &bytes)?;
        write_json(
            &trace.join(format!("{phase}-{name}-receipt.json")),
            &json!({"path":path,"bytes":size,"sha256":hex(&Sha256::digest(&bytes)),"before":before,"after":after}),
        )?;
        copies.push(bytes);
    }
    let raw = identity::parse(&copies[0])?;
    let plist = identity::parse(&copies[1])?;
    raw.ensure_matches(&plist)?;
    Ok(raw)
}

/// A file's attributes; unlike `afc_stat::attributes`, a missing file is an
/// error here.
async fn attributes(afc: &mut AfcClient, path: &str, deadline: Instant) -> Result<Attributes> {
    afc_stat::attributes(afc, path, deadline, None, "stat prefs")
        .await?
        .context("stat prefs: object not found")
}
fn unix_time_ns() -> Result<String> {
    Ok(SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos()
        .to_string())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn identity_json(identity: &IndexIdentity) -> Value {
    json!({"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)})
}
fn catalog_json(binding: &CatalogBinding) -> Value {
    json!({"database_sha256":hex(&binding.database_sha256),"database_bytes":binding.database_bytes,
    "wal_sha256":binding.wal_sha256.map(|hash|hex(&hash)),"wal_bytes":binding.wal_bytes,"persisted_media_anchor":binding.persisted_media_anchor})
}
fn track_json(row: &TrackAudio) -> Value {
    json!({"sync_id":format!("{:016X}",row.sync_id),"afc_path":row.afc_path,"attributes":row.attributes,"fingerprint":fingerprint_json(&row.fingerprint)})
}
fn other_json(row: &OtherAudio) -> Value {
    let codec = crate::engine::state::audio_index::codec_json(&row.codec);
    json!({"sync_id":format!("{:016X}",row.sync_id),"afc_path":row.afc_path,"attributes":row.attributes,"file_bytes":row.file_bytes,"full_file_sha256":hex(&row.full_file_sha256),"codec":codec})
}
fn catalog_metadata(catalog: &Catalog) -> Value {
    json!({"database_pid":format!("{:016X}",catalog.database_pid()),"primary_container_pid":format!("{:016X}",catalog.primary_container_pid()),
    "tracks":catalog.tracks().iter().map(|track|json!({"sync_id":format!("{:016X}",track.sync_id),"title":track.metadata.title,"artist":track.metadata.artist,
    "album":track.metadata.album,"album_artist":track.metadata.album_artist,"track_number":track.metadata.track_number,"disc_number":track.metadata.disc_number,
    "total_time_ms":track.metadata.total_time_ms,"file_size":track.metadata.file_size,"afc_path":track.afc_path})).collect::<Vec<_>>()})
}
fn append_progress(output: &mut File, digest: &mut Sha256, record: &Value) -> Result<()> {
    struct Hashed<'a> {
        output: &'a mut File,
        digest: &'a mut Sha256,
    }
    impl Write for Hashed<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = self.output.write(bytes)?;
            self.digest.update(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }
    {
        let mut writer = Hashed {
            output: &mut *output,
            digest: &mut *digest,
        };
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")?;
    }
    // Synchronized once when the pass finishes: until complete-index.json
    // exists this file is partial evidence only, and an fsync per record cost
    // about a second per pass.
    Ok(())
}

fn publish_complete_index(
    trace: &Path,
    body: &Value,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<()> {
    let pending = trace.join("complete-index.pending.json");
    check_capture_control(deadline, cancelled)?;
    write_json(&pending, body)?;
    check_capture_control(deadline, cancelled)?;
    // Only a complete, fsynced sink can publish the existing completion marker.
    fs::hard_link(&pending, trace.join("complete-index.json"))?;
    fs::remove_file(pending)?;
    File::open(trace)?.sync_all()?;
    check_capture_control(deadline, cancelled)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut file = private_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_output_and_budget_guards_precede_discovery() {
        assert!(
            capture(&std::env::temp_dir(), Duration::from_secs(1), || async {
                panic!("no connector")
            })
            .await
            .is_err()
        );
        assert!(
            capture(&std::env::temp_dir(), Duration::from_secs(1801), || async {
                panic!("no connector")
            })
            .await
            .is_err()
        );
    }
    #[test]
    fn expected_identity_rejects_device_and_library_changes_before_catalogue() {
        let expected = IndexIdentity {
            device_identifier_sha256: [1; 32],
            library_identity_sha256: [2; 32],
        };
        ensure_expected_identity(Some(&expected), &expected).unwrap();
        for observed in [
            IndexIdentity {
                device_identifier_sha256: [3; 32],
                ..expected.clone()
            },
            IndexIdentity {
                library_identity_sha256: [3; 32],
                ..expected.clone()
            },
        ] {
            assert!(ensure_expected_identity(Some(&expected), &observed).is_err());
            ensure_expected_identity(None, &observed).unwrap();
        }
    }
    #[test]
    fn full_typed_fingerprint_is_retained_without_flattening_audio_components() {
        let path = crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a");
        let fingerprint = music_protocol::media::fingerprint::read(&path).unwrap();
        let json = fingerprint_json(&fingerprint);
        assert_eq!(json["full_file_sha256"], hex(&fingerprint.full_file_sha256));
        assert_eq!(json["audio"]["sha256"], hex(&fingerprint.audio.sha256));
        assert_eq!(
            json["audio"]["sample_sizes_sha256"],
            hex(&fingerprint.audio.sample_sizes_sha256)
        );
        assert_eq!(json["audio"]["duration_ticks"], 2205);
        assert_eq!(json["audio"]["timescale"], 44100);
    }
}

#[cfg(test)]
mod retry_tests;
