use super::*;
use crate::engine::verification::catalogue_capture;
use music_protocol::protocol::trace::private_write;
use std::{
    cell::Cell,
    os::unix::fs::symlink,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "capture-retry-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn files(path: &Path, value: u8) -> Result<[u8; 32]> {
    fs::DirBuilder::new().mode(0o700).create(path.join("afc"))?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path.join("catalog"))?;
    for name in [
        "iTunesPrefs",
        "iTunesPrefs.plist",
        "iTunesCDB",
        "ArtworkDB",
        "catalog/MediaLibrary.sqlitedb",
        "catalog/MediaLibrary.sqlitedb-wal",
        "afc/receipt.plist",
    ] {
        private_write(&path.join(name), &[value])?;
    }
    private_write(&path.join("state-receipt.json"), &[value])?;
    Ok(Sha256::digest([value]).into())
}

#[tokio::test]
async fn one_typed_drift_recaptures_whole_state_without_mixing_or_renewing_deadline() {
    let temp = Temp::new();
    let trace = temp.0.join("after");
    let calls = Cell::new(0u8);
    let deadline = Instant::now() + Duration::from_secs(5);
    let selected = capture_attempts(&trace, deadline, |path, observed| {
        assert_eq!(observed, deadline);
        let n = calls.get() + 1;
        calls.set(n);
        async move {
            let hash = files(&path, n)?;
            if n == 1 {
                return Err(
                    anyhow::Error::from(catalogue_capture::CatalogCaptureDrift::fixture())
                        .context("retained nested catalog capture"),
                );
            }
            Ok((n, hash))
        }
    })
    .await
    .unwrap();
    assert_eq!(selected, 2);
    assert_eq!(calls.get(), 2);
    let failed = temp.0.join("after.capture-attempts/attempt-0001");
    for name in [
        "iTunesPrefs",
        "iTunesPrefs.plist",
        "iTunesCDB",
        "ArtworkDB",
        "catalog/MediaLibrary.sqlitedb",
        "catalog/MediaLibrary.sqlitedb-wal",
        "afc/receipt.plist",
    ] {
        assert_eq!(fs::read(trace.join(name)).unwrap(), [2]);
        assert_eq!(fs::read(failed.join(name)).unwrap(), [1]);
    }
    assert!(failed.join("error.txt").is_file());
    assert!(!temp.0.join("after.capture-attempts/attempt-0002").exists());
    let selection: serde_json::Value = serde_json::from_slice(
        &fs::read(temp.0.join("after.capture-attempts/selection.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(selection["selected_attempt"], 2);
    assert_eq!(selection["failed_attempts"][0]["directory"], "attempt-0001");
    assert_eq!(selection["failed_attempts"][0]["catalog_drift"], true);
    assert_eq!(
        selection["failed_attempts"][0]["receipt_sha256"],
        hex(&Sha256::digest(
            fs::read(failed.join("capture-attempt.json")).unwrap()
        ))
    );
    assert_eq!(selection["deadline_renewed"], false);
}

#[tokio::test]
async fn two_drifts_stop_and_keep_both_failed_trees() {
    let temp = Temp::new();
    let calls = Cell::new(0u8);
    let result: Result<()> = capture_attempts(
        &temp.0.join("after"),
        Instant::now() + Duration::from_secs(5),
        |path, _| {
            let n = calls.get() + 1;
            calls.set(n);
            async move {
                files(&path, n)?;
                Err(catalogue_capture::CatalogCaptureDrift::fixture().into())
            }
        },
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .downcast_ref::<catalogue_capture::CatalogCaptureDrift>()
            .is_some()
    );
    assert_eq!(calls.get(), 2);
    assert!(!temp.0.join("after").exists());
    for n in 1..=2 {
        assert_eq!(
            fs::read(
                temp.0
                    .join(format!("after.capture-attempts/attempt-{n:04}/iTunesPrefs"))
            )
            .unwrap(),
            [n]
        );
    }
}

fn dropped_connection() -> anyhow::Error {
    anyhow::anyhow!(idevice::IdeviceError::Socket(std::io::Error::from(
        std::io::ErrorKind::BrokenPipe
    )))
    .context("AFC operation failed; journal retained")
}

#[tokio::test]
async fn one_dropped_connection_recaptures_once_and_a_second_drop_stops() {
    for drops in [1u8, 2] {
        let temp = Temp::new();
        let trace = temp.0.join("after");
        let calls = Cell::new(0u8);
        let result = capture_attempts(
            &trace,
            Instant::now() + Duration::from_secs(5),
            |path, _| {
                let n = calls.get() + 1;
                calls.set(n);
                async move {
                    let hash = files(&path, n)?;
                    if n <= drops {
                        return Err(dropped_connection());
                    }
                    Ok((n, hash))
                }
            },
        )
        .await;
        assert_eq!(calls.get(), 2);
        let first: serde_json::Value = serde_json::from_slice(
            &fs::read(
                temp.0
                    .join("after.capture-attempts/attempt-0001/capture-attempt.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(first["connection_dropped"], true);
        if drops == 1 {
            assert_eq!(result.unwrap(), 2);
            assert_eq!(fs::read(trace.join("iTunesPrefs")).unwrap(), [2]);
            let selection: serde_json::Value = serde_json::from_slice(
                &fs::read(temp.0.join("after.capture-attempts/selection.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(selection["failed_attempts"][0]["connection_dropped"], true);
            assert_eq!(selection["failed_attempts"][0]["catalog_drift"], false);
        } else {
            assert!(music_protocol::protocol::afc::dropped_by_phone(
                &result.unwrap_err()
            ));
            assert!(!trace.exists());
        }
    }
}

#[tokio::test]
async fn text_lookalike_transport_parse_identity_and_postcondition_errors_never_retry() {
    for message in [
        "catalog files changed across capture; no catalog exported",
        "unexpected AFC read response",
        "premature EOF",
        "WAL checksum mismatch",
        "fresh library identity differs",
        "postcondition anchor mismatch",
    ] {
        let temp = Temp::new();
        let calls = Cell::new(0);
        let result: Result<()> = capture_attempts(
            &temp.0.join("after"),
            Instant::now() + Duration::from_secs(5),
            |path, _| {
                calls.set(calls.get() + 1);
                async move {
                    files(&path, 1)?;
                    anyhow::bail!(message)
                }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
        assert!(!temp.0.join("after").exists());
        assert!(
            temp.0
                .join("after.capture-attempts/attempt-0001/error.txt")
                .is_file()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn expired_and_consumed_original_deadlines_never_start_an_extra_attempt() {
    let temp = Temp::new();
    let calls = Cell::new(0);
    let result: Result<()> = capture_attempts(&temp.0.join("expired"), Instant::now(), |_, _| {
        calls.set(calls.get() + 1);
        async { anyhow::bail!("must not be polled") }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(calls.get(), 0);
    for drift in [true, false] {
        let trace = temp.0.join(format!("short-{drift}"));
        let result: Result<()> = capture_attempts(
            &trace,
            Instant::now() + Duration::from_millis(100),
            |path, _| {
                calls.set(calls.get() + 1);
                async move {
                    files(&path, 1)?;
                    if drift {
                        return Err(catalogue_capture::CatalogCaptureDrift::fixture().into());
                    }
                    std::future::pending::<Result<((), [u8; 32])>>().await
                }
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!trace.exists());
    }
    assert_eq!(calls.get(), 2);
}

#[tokio::test]
async fn existing_targets_and_symlink_parents_stop_before_capture() {
    let temp = Temp::new();
    let calls = Cell::new(0);
    fs::create_dir(temp.0.join("existing")).unwrap();
    symlink(temp.0.join("existing"), temp.0.join("link")).unwrap();
    for trace in [
        temp.0.join("existing"),
        temp.0.join("link"),
        temp.0.join("link/child"),
    ] {
        let result: Result<()> =
            capture_attempts(&trace, Instant::now() + Duration::from_secs(5), |_, _| {
                calls.set(calls.get() + 1);
                async { anyhow::bail!("must not be polled") }
            })
            .await;
        assert!(result.is_err());
    }
    assert_eq!(calls.get(), 0);
    assert_eq!(fs::read_dir(temp.0.join("existing")).unwrap().count(), 0);
}

#[tokio::test]
async fn racing_empty_target_or_symlink_is_never_replaced() {
    for link in [false, true] {
        let temp = Temp::new();
        let trace = temp.0.join("after");
        let target = trace.clone();
        let outside = temp.0.join("outside");
        fs::create_dir(&outside).unwrap();
        let result = capture_attempts(
            &trace,
            Instant::now() + Duration::from_secs(5),
            |path, _| {
                let target = target.clone();
                let outside = outside.clone();
                async move {
                    let hash = files(&path, 1)?;
                    if link {
                        symlink(&outside, &target)?;
                    } else {
                        fs::create_dir(&target)?;
                    }
                    Ok(((), hash))
                }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(trace.is_symlink(), link);
        assert_eq!(fs::read_dir(&trace).unwrap().count(), 0);
        assert_eq!(
            fs::read(
                temp.0
                    .join("after.capture-attempts/attempt-0001/iTunesPrefs")
            )
            .unwrap(),
            [1]
        );
    }
}

#[tokio::test]
async fn parent_or_attempt_swap_cannot_redirect_writes_or_publish() {
    for parent_swap in [true, false] {
        let temp = Temp::new();
        let parent = temp.0.join("parent");
        fs::create_dir(&parent).unwrap();
        let trace = parent.join("after");
        let parent_copy = parent.clone();
        let base = temp.0.clone();
        let result = capture_attempts(
            &trace,
            Instant::now() + Duration::from_secs(5),
            |path, _| {
                let parent = parent_copy.clone();
                let base = base.clone();
                async move {
                    if parent_swap {
                        fs::rename(&parent, base.join("held-parent"))?;
                        fs::create_dir(&parent)?;
                    } else {
                        let slot = parent.join("after.capture-attempts/attempt-0001");
                        fs::rename(&slot, base.join("held-attempt"))?;
                        fs::create_dir(base.join("outside"))?;
                        symlink(base.join("outside"), &slot)?;
                    }
                    let hash = files(&path, 1)?;
                    Ok(((), hash))
                }
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!trace.exists());
        let held = if parent_swap {
            temp.0
                .join("held-parent/after.capture-attempts/attempt-0001")
        } else {
            temp.0.join("held-attempt")
        };
        assert_eq!(fs::read(held.join("iTunesPrefs")).unwrap(), [1]);
        assert_eq!(
            fs::read_dir(if parent_swap {
                parent
            } else {
                temp.0.join("outside")
            })
            .unwrap()
            .count(),
            0
        );
    }
}

#[tokio::test]
async fn actual_catalog_parser_reads_fd_anchored_sources_before_whole_tree_publication() {
    let temp = Temp::new();
    let trace = temp.0.join("after");
    let observed = capture_attempts(
        &trace,
        Instant::now() + Duration::from_secs(5),
        |path, _| async move {
            fs::create_dir(path.join("catalog"))?;
            let database = path.join("catalog/MediaLibrary.sqlitedb");
            let expected =
                music_protocol::test_support::catalogue::copy_closed_collection_fixture(&database);
            let catalog = catalogue_capture::parse_copied_snapshot(&database, None)?;
            assert_eq!(catalog.source_hashes().database, expected);
            assert_eq!(catalog.persisted_media_anchor(), 12);
            assert_eq!(catalog.tracks().len(), 1);
            Ok((catalog, expected))
        },
    )
    .await
    .unwrap();
    assert_eq!(observed.tracks().len(), 1);
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(
            fs::read(trace.join("catalog/MediaLibrary.sqlitedb")).unwrap()
        )),
        observed.source_hashes().database
    );
}

#[tokio::test(start_paused = true)]
async fn blocking_local_parse_cannot_return_success_after_original_deadline() {
    let temp = Temp::new();
    let trace = temp.0.join("after");
    let calls = Cell::new(0);
    let result = capture_attempts(
        &trace,
        Instant::now() + Duration::from_millis(100),
        |path, _| {
            calls.set(calls.get() + 1);
            async move {
                let hash = files(&path, 1)?;
                // Exclude private filesystem setup, then let the non-yielding
                // parse consume the original deadline on the real clock.
                tokio::time::resume();
                std::thread::sleep(Duration::from_millis(150));
                Ok(((), hash))
            }
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().to_string(),
        "fresh state capture original deadline expired before publication"
    );
    assert_eq!(calls.get(), 1);
    assert!(!trace.exists());
    assert!(
        temp.0
            .join("after.capture-attempts/attempt-0001/state-receipt.json")
            .is_file()
    );
}

#[tokio::test]
async fn actual_catalog_parse_error_is_terminal_and_original_bytes_remain() {
    let temp = Temp::new();
    let trace = temp.0.join("after");
    let calls = Cell::new(0);
    let result = capture_attempts(
        &trace,
        Instant::now() + Duration::from_secs(5),
        |path, _| {
            calls.set(calls.get() + 1);
            async move {
                let digest = files(&path, 1)?;
                let catalog = catalogue_capture::parse_copied_snapshot(
                    &path.join("catalog/MediaLibrary.sqlitedb"),
                    None,
                )?;
                Ok((catalog, digest))
            }
        },
    )
    .await;
    let error = result.err().unwrap();
    assert!(
        error
            .downcast_ref::<catalogue_capture::CatalogCaptureDrift>()
            .is_none()
    );
    assert_eq!(calls.get(), 1);
    assert!(!trace.exists());
    assert_eq!(
        fs::read(
            temp.0
                .join("after.capture-attempts/attempt-0001/catalog/MediaLibrary.sqlitedb")
        )
        .unwrap(),
        [1]
    );
}

#[tokio::test]
async fn controlled_attempt_cancelled_before_entry_creates_nothing() {
    let temp = Temp::new();
    let trace = temp.0.join("controlled");
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(true)),
    };
    let calls = Cell::new(0);
    let result = capture_attempts_options(&trace, control.deadline, Some(&control), |_, _| {
        calls.set(calls.get() + 1);
        async { Ok((1_u8, [0; 32])) }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(calls.get(), 0);
    assert!(!trace.exists());
    assert!(!temp.0.join("controlled.capture-attempts").exists());
}

#[tokio::test]
async fn controlled_original_arc_flip_retains_one_prefix_without_retry_or_selection() {
    let temp = Temp::new();
    let trace = temp.0.join("controlled");
    let flag = Arc::new(AtomicBool::new(false));
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: flag.clone(),
    };
    let calls = Cell::new(0);
    let result =
        capture_attempts_options(&trace, control.deadline, Some(&control), |path, seen| {
            assert_eq!(seen, control.deadline);
            calls.set(calls.get() + 1);
            let flag = flag.clone();
            async move {
                files(&path, 7)?;
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                Err::<(u8, [u8; 32]), _>(catalogue_capture::CatalogCaptureDrift::fixture().into())
            }
        })
        .await;
    assert!(result.is_err());
    assert_eq!(calls.get(), 1);
    let storage = temp.0.join("controlled.capture-attempts");
    assert!(storage.join("attempt-0001/error.txt").is_file());
    assert!(storage.join("attempt-0001/capture-attempt.json").is_file());
    assert!(!storage.join("attempt-0002").exists());
    assert!(!storage.join("selection.json").exists());
    assert!(!trace.exists());
    assert!(
        std::fs::metadata(storage.join("attempt-0001/error.txt"))
            .unwrap()
            .len()
            <= 2049
    );
}

#[test]
fn controlled_finish_original_arc_cancel_after_success_refuses() {
    let flag = Arc::new(AtomicBool::new(false));
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: flag.clone(),
    };
    let success = Ok(7_u8);
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(finish_controlled(&control, success).is_err());
}

#[test]
fn controlled_finish_original_deadline_expired_after_success_refuses() {
    let control = CaptureControl {
        deadline: Instant::now() - Duration::from_millis(1),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let success = Ok(7_u8);
    assert!(finish_controlled(&control, success).is_err());
}
