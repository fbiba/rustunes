use super::*;
use crate::engine::state::audio_index;
use music_protocol::protocol::trace::private_write;
use std::cell::Cell;

struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "initial-capture-retry-{}-{}",
            std::process::id(),
            unix_time_ns().unwrap(),
        ));
        DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn initial_evidence(trace: &Path, fresh: bool) -> Result<()> {
    for name in ["before-iTunesPrefs", "before-iTunesPrefs.plist"] {
        private_write(
            &trace.join(name),
            if fresh {
                b"fresh prefs"
            } else {
                b"initial prefs"
            },
        )?;
        write_json(
            &trace.join(format!("{name}-receipt.json")),
            &json!({"fresh":fresh}),
        )?;
    }
    DirBuilder::new()
        .mode(0o700)
        .create(trace.join("catalog"))?;
    private_write(&trace.join("catalog/MediaLibrary.sqlitedb"), b"database")?;
    private_write(
        &trace.join("catalog/MediaLibrary.sqlitedb-wal"),
        if fresh { b"" } else { b"wal" },
    )?;
    Ok(())
}

#[tokio::test]
async fn initial_wal_drift_recaptures_once_and_retains_failed_evidence() {
    let temp = Temp::new();
    let trace = &temp.0;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(3);
    let control = &ReadControl {
        deadline,
        cancelled: &cancelled,
    };
    let calls = Cell::new(0);
    let mut retry_attempted = false;
    let result = capture_attempts(
        trace,
        deadline,
        &cancelled,
        &mut retry_attempted,
        |mut progress| {
            let fresh = calls.get() == 1;
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, fresh)?;
                audio_index::tests::truncated_wal_attempt(fresh, trace, control, |event| {
                    if matches!(event, IndexProgress::Started { .. }) {
                        progress.write_all(b"{\"event\":\"started\"}\n")?;
                        progress.sync_all()?;
                    }
                    Ok(())
                })
                .await?;
                write_json(
                    &trace.join("complete-index.json"),
                    &json!({"complete":true}),
                )
            }
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "fresh read-only recapture must succeed: {result:?}"
    );
    assert_eq!(calls.get(), 2);
    assert!(retry_attempted);
    let failed = trace.join("failed-attempt-1");
    assert_eq!(
        fs::read(failed.join("before-iTunesPrefs")).unwrap(),
        b"initial prefs"
    );
    assert_eq!(
        fs::read(failed.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
        b"wal"
    );
    assert!(fs::read(failed.join("progress.jsonl")).unwrap().is_empty());
    assert!(!failed.join("complete-index.json").exists());
    assert!(failed.join("outcome.json").is_file());
    assert_eq!(
        fs::read(trace.join("before-iTunesPrefs")).unwrap(),
        b"fresh prefs"
    );
    assert!(
        fs::read(trace.join("catalog/MediaLibrary.sqlitedb-wal"))
            .unwrap()
            .is_empty()
    );
    assert!(trace.join("complete-index.json").is_file());
    assert!(!fs::read(trace.join("progress.jsonl")).unwrap().is_empty());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&failed).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(failed.join("progress.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn second_initial_drift_stops_with_both_attempts_retained() {
    let temp = Temp::new();
    let trace = &temp.0;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(3);
    let control = &ReadControl {
        deadline,
        cancelled: &cancelled,
    };
    let calls = Cell::new(0);
    let mut retried = false;
    let error = capture_attempts(trace, deadline, &cancelled, &mut retried, |_| {
        calls.set(calls.get() + 1);
        async move {
            initial_evidence(trace, false)?;
            audio_index::tests::truncated_wal_attempt(false, trace, control, |_| {
                panic!("both initial guards fail before progress")
            })
            .await
        }
    })
    .await
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<audio_index::InitialCatalogGuardDrift>()
            .is_some()
    );
    assert_eq!(calls.get(), 2);
    assert!(retried);
    for path in [trace.to_owned(), trace.join("failed-attempt-1")] {
        assert_eq!(
            fs::read(path.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
            b"wal"
        );
        assert!(fs::read(path.join("progress.jsonl")).unwrap().is_empty());
        assert!(!path.join("complete-index.json").exists());
    }
}

#[tokio::test]
async fn generic_io_and_unretained_catalogue_drift_do_not_retry() {
    for catalogue_drift in [false, true] {
        let temp = Temp::new();
        let calls = Cell::new(0);
        let mut retried = false;
        let error = capture_attempts(
            &temp.0,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
            &mut retried,
            |_| {
                calls.set(calls.get() + 1);
                async move {
                    if catalogue_drift {
                        Err(crate::engine::verification::catalogue_capture::CatalogCaptureDrift::fixture().into())
                    } else {
                        Err(std::io::Error::other("live catalog WAL presence/size differs").into())
                    }
                }
            },
        )
        .await
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<audio_index::InitialCatalogGuardDrift>()
                .is_none()
        );
        assert_eq!(calls.get(), 1);
        assert!(!retried);
        assert!(!temp.0.join("failed-attempt-1").exists());
        assert!(!temp.0.join("complete-index.json").exists());
    }
}

#[tokio::test(start_paused = true)]
async fn retry_shares_original_deadline_and_cancellation_during_second_attempt() {
    for cancel_second in [false, true] {
        let temp = Temp::new();
        let trace = &temp.0;
        let cancelled = &AtomicBool::new(false);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(400);
        let control = &ReadControl {
            deadline,
            cancelled,
        };
        let calls = Cell::new(0);
        let mut retried = false;
        let result = capture_attempts(trace, deadline, cancelled, &mut retried, |_| {
            let second = calls.get() == 1;
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, second)?;
                if second {
                    if cancel_second {
                        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    panic!("second attempt must be dropped by original control");
                }
                // Spend most of the original budget before the retry: renewing
                // it to a fresh full duration would exceed the elapsed bound.
                tokio::time::sleep(Duration::from_millis(250)).await;
                audio_index::tests::truncated_wal_attempt(false, trace, control, |_| {
                    panic!("initial failure must not publish progress")
                })
                .await
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 2);
        assert!(retried);
        assert!(started.elapsed() < Duration::from_millis(550));
        assert!(trace.join("failed-attempt-1/outcome.json").exists());
        assert!(!trace.join("complete-index.json").exists());
        assert!(fs::read(trace.join("progress.jsonl")).unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_or_expired_budget_after_first_guard_prevents_retry() {
    for cancel_first in [false, true] {
        let temp = Temp::new();
        let trace = &temp.0;
        let cancelled = &AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_millis(100);
        let control = &ReadControl {
            deadline,
            cancelled,
        };
        let calls = Cell::new(0);
        let mut retried = false;
        let result = capture_attempts(trace, deadline, cancelled, &mut retried, |_| {
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, false)?;
                let result =
                    audio_index::tests::truncated_wal_attempt(false, trace, control, |_| {
                        panic!("initial failure must not publish progress")
                    })
                    .await;
                assert!(
                    result
                        .as_ref()
                        .unwrap_err()
                        .downcast_ref::<audio_index::InitialCatalogGuardDrift>()
                        .is_some()
                );
                assert!(Instant::now() < deadline);
                if cancel_first {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                } else {
                    // Fixture writes and their fsyncs must not consume the short
                    // guard budget. Resume only for this blocking expiry proof;
                    // no yield lets the outer timeout replace the first guard.
                    tokio::time::resume();
                    std::thread::sleep(Duration::from_millis(125));
                    assert!(Instant::now() >= deadline);
                }
                result
            }
        })
        .await;
        if !cancel_first {
            tokio::time::pause();
        }
        assert!(result.is_err());
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(if cancel_first {
            "cancelled"
        } else {
            "deadline"
        }));
        assert_eq!(calls.get(), 1);
        assert!(!retried);
        assert!(!trace.join("complete-index.json").exists());
        assert_eq!(
            fs::read(trace.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
            b"wal"
        );
    }
}

#[test]
fn fresh_retry_cannot_change_the_initial_library_identity() {
    let session = OnceLock::new();
    let original = IndexIdentity {
        device_identifier_sha256: [1; 32],
        library_identity_sha256: [2; 32],
    };
    bind_capture_identity(None, &session, &original).unwrap();
    bind_capture_identity(None, &session, &original).unwrap();
    for changed in [
        IndexIdentity {
            device_identifier_sha256: [3; 32],
            ..original.clone()
        },
        IndexIdentity {
            library_identity_sha256: [3; 32],
            ..original.clone()
        },
    ] {
        assert!(bind_capture_identity(None, &session, &changed).is_err());
    }
    assert!(session.get().unwrap() == &original);
}

#[tokio::test]
async fn completed_catalogue_copy_drift_recaptures_before_audio_and_retains_evidence() {
    let temp = Temp::new();
    let trace = &temp.0;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(3);
    let calls = Cell::new(0);
    let mut retried = false;
    let result = capture_attempts(trace, deadline, &cancelled, &mut retried, |mut progress| {
        let fresh = calls.get() == 1;
        calls.set(calls.get() + 1);
        async move {
            initial_evidence(trace, fresh)?;
            if !fresh {
                private_write(
                    &trace.join("catalog/copies.plist"),
                    b"retained completed and closed catalogue copy evidence",
                )?;
                return Err(anyhow::Error::new(
                    crate::engine::verification::catalogue_capture::CatalogCaptureDrift::fixture(),
                )
                .context("post-transfer initial catalogue copy"));
            }
            progress.write_all(b"{\"event\":\"started\"}\n")?;
            progress.sync_all()?;
            write_json(
                &trace.join("complete-index.json"),
                &json!({"complete":true}),
            )
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "a completed initial catalogue drift must permit one fresh read: {result:?}"
    );
    assert_eq!(calls.get(), 2);
    assert!(retried);
    let failed = trace.join("failed-attempt-1");
    assert_eq!(
        fs::read(failed.join("before-iTunesPrefs")).unwrap(),
        b"initial prefs"
    );
    assert_eq!(
        fs::read(failed.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
        b"wal"
    );
    assert_eq!(
        fs::read(failed.join("catalog/copies.plist")).unwrap(),
        b"retained completed and closed catalogue copy evidence"
    );
    assert!(fs::read(failed.join("progress.jsonl")).unwrap().is_empty());
    assert!(!failed.join("complete-index.json").exists());
    let outcome: serde_json::Value =
        serde_json::from_slice(&fs::read(failed.join("outcome.json")).unwrap()).unwrap();
    assert_eq!(outcome["initial_catalog_guard_drift"], false);
    assert_eq!(
        outcome["catalog_capture_drift_files"],
        json!(["MediaLibrary.sqlitedb-wal"])
    );
    assert_eq!(outcome["deadline_renewed"], false);
    assert_eq!(outcome["phone_writes"], 0);
    assert_eq!(
        fs::read(trace.join("before-iTunesPrefs")).unwrap(),
        b"fresh prefs"
    );
    assert!(trace.join("complete-index.json").is_file());
}

#[tokio::test]
async fn repeated_catalogue_copy_drift_stops_after_two_retained_attempts() {
    let temp = Temp::new();
    let trace = &temp.0;
    let calls = Cell::new(0);
    let mut retried = false;
    let error = capture_attempts(
        trace,
        Instant::now() + Duration::from_secs(3),
        &AtomicBool::new(false),
        &mut retried,
        |_| {
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, false)?;
                Err(
                    crate::engine::verification::catalogue_capture::CatalogCaptureDrift::fixture()
                        .into(),
                )
            }
        },
    )
    .await
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::engine::verification::catalogue_capture::CatalogCaptureDrift>()
            .is_some()
    );
    assert_eq!(calls.get(), 2);
    assert!(retried);
    for path in [temp.0.clone(), temp.0.join("failed-attempt-1")] {
        assert_eq!(
            fs::read(path.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
            b"wal"
        );
        assert!(fs::read(path.join("progress.jsonl")).unwrap().is_empty());
        assert!(!path.join("complete-index.json").exists());
    }
    assert!(!temp.0.join("failed-attempt-2").exists());
}

#[tokio::test]
async fn catalogue_copy_drift_with_audio_progress_or_extra_artifacts_does_not_retry() {
    for audio_started in [false, true] {
        let temp = Temp::new();
        let trace = &temp.0;
        let calls = Cell::new(0);
        let mut retried = false;
        let error = capture_attempts(
            trace,
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(false),
            &mut retried,
            |mut progress| {
                calls.set(calls.get() + 1);
                async move {
                    initial_evidence(trace, false)?;
                    if audio_started {
                        progress.write_all(b"{\"event\":\"started\"}\n")?;
                        progress.sync_all()?;
                    } else {
                        private_write(&trace.join("unexpected-after-read"), b"partial")?;
                    }
                    Err(crate::engine::verification::catalogue_capture::CatalogCaptureDrift::fixture().into())
                }
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains(if audio_started {
            "audio progress"
        } else {
            "unexpected capture artifacts"
        }));
        assert_eq!(calls.get(), 1);
        assert!(!retried);
        assert!(!temp.0.join("failed-attempt-1").exists());
        assert!(!temp.0.join("complete-index.json").exists());
        assert_eq!(
            fs::read(temp.0.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
            b"wal"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn catalogue_copy_retry_keeps_original_deadline_and_cancellation() {
    for cancel_second in [false, true] {
        let temp = Temp::new();
        let trace = &temp.0;
        let cancelled = &AtomicBool::new(false);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(400);
        let calls = Cell::new(0);
        let mut retried = false;
        let result = capture_attempts(trace, deadline, cancelled, &mut retried, |_| {
            let second = calls.get() == 1;
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, second)?;
                if second {
                    if cancel_second {
                        cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    panic!("second catalogue read must stop at the original control");
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                Err(
                    crate::engine::verification::catalogue_capture::CatalogCaptureDrift::fixture()
                        .into(),
                )
            }
        })
        .await;
        assert!(result.is_err());
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains(if cancel_second {
            "cancelled"
        } else {
            "deadline"
        }));
        assert_eq!(calls.get(), 2);
        assert!(retried);
        assert!(started.elapsed() < Duration::from_millis(550));
        assert!(trace.join("failed-attempt-1/outcome.json").exists());
        assert!(!trace.join("complete-index.json").exists());
    }
}

#[tokio::test]
async fn late_catalogue_drift_after_a_fingerprint_pass_recaptures_once() {
    let temp = Temp::new();
    let trace = &temp.0;
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(3);
    let control = &ReadControl {
        deadline,
        cancelled: &cancelled,
    };
    let calls = Cell::new(0);
    let mut retried = false;
    let result = capture_attempts(trace, deadline, &cancelled, &mut retried, |mut progress| {
        let fresh = calls.get() == 1;
        calls.set(calls.get() + 1);
        async move {
            initial_evidence(trace, fresh)?;
            if !fresh {
                return audio_index::tests::late_drift_attempt(trace, control, |event| {
                    if matches!(event, IndexProgress::Fingerprinted(_)) {
                        progress.write_all(b"{\"event\":\"fingerprinted\"}\n")?;
                        progress.sync_all()?;
                    }
                    Ok(())
                })
                .await;
            }
            progress.write_all(b"{\"event\":\"started\"}\n")?;
            progress.sync_all()?;
            write_json(
                &trace.join("complete-index.json"),
                &json!({"complete":true}),
            )
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "a late drift must permit one fresh read: {result:?}"
    );
    assert_eq!(calls.get(), 2);
    assert!(retried);
    let failed = trace.join("failed-attempt-1");
    assert_eq!(
        fs::read(failed.join("progress.jsonl")).unwrap(),
        b"{\"event\":\"fingerprinted\"}\n"
    );
    assert_eq!(
        fs::read(failed.join("catalog/MediaLibrary.sqlitedb-wal")).unwrap(),
        b"wal"
    );
    assert!(!failed.join("complete-index.json").exists());
    let outcome: serde_json::Value =
        serde_json::from_slice(&fs::read(failed.join("outcome.json")).unwrap()).unwrap();
    assert_eq!(outcome["late_catalog_guard_drift"], true);
    assert_eq!(outcome["initial_catalog_guard_drift"], false);
    assert_eq!(outcome["phone_writes"], 0);
    assert_eq!(
        fs::read(trace.join("before-iTunesPrefs")).unwrap(),
        b"fresh prefs"
    );
    assert!(trace.join("complete-index.json").is_file());
}

#[tokio::test]
async fn second_late_drift_or_extra_artifacts_stop_the_capture() {
    for extra in [false, true] {
        let temp = Temp::new();
        let trace = &temp.0;
        let cancelled = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(3);
        let control = &ReadControl {
            deadline,
            cancelled: &cancelled,
        };
        let calls = Cell::new(0);
        let mut retried = false;
        let error = capture_attempts(trace, deadline, &cancelled, &mut retried, |_| {
            calls.set(calls.get() + 1);
            async move {
                initial_evidence(trace, false)?;
                if extra {
                    private_write(&trace.join("unexpected-after-read"), b"partial")?;
                }
                audio_index::tests::late_drift_attempt(trace, control, |_| Ok(())).await
            }
        })
        .await
        .unwrap_err();
        if extra {
            assert!(format!("{error:#}").contains("unexpected capture artifacts"));
            assert_eq!(calls.get(), 1);
            assert!(!retried);
            assert!(!trace.join("failed-attempt-1").exists());
        } else {
            assert!(
                error
                    .downcast_ref::<audio_index::LateCatalogGuardDrift>()
                    .is_some()
            );
            assert_eq!(calls.get(), 2);
            assert!(retried);
            assert!(trace.join("failed-attempt-1/outcome.json").is_file());
            assert!(!trace.join("failed-attempt-2").exists());
        }
        assert!(!trace.join("complete-index.json").exists());
    }
}
