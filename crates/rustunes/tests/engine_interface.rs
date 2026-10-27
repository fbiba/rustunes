//! The application library's typed engine is usable without clap or the binary.
use rustunes::engine::{
    ChangeApplyRequest, Config, CoverRequest, DeletionMode, DeletionRequest, DeviceConfig, Engine,
    Event, ParseTrackIdError, PlanRequest, RecoveryRequest, SelectionOptions, SyncRequest, TrackId,
    VerificationRequest,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[test]
fn track_ids_preserve_numeric_identity_and_canonical_hex() {
    for (text, value, canonical) in [
        ("0000000000000001", 1, "0000000000000001"),
        ("0123456789AbCdEf", 0x0123456789abcdef, "0123456789abcdef"),
        ("FFFFFFFFFFFFFFFF", u64::MAX, "ffffffffffffffff"),
    ] {
        let id: TrackId = text.parse().unwrap();
        assert_eq!(id.get(), value);
        assert_eq!(id.to_string(), canonical);
        assert_eq!(TrackId::try_from(value).unwrap(), id);
        assert_eq!(canonical.parse::<TrackId>().unwrap(), id);
    }
}

#[test]
fn track_ids_reject_invalid_text_and_zero_at_the_public_boundary() {
    for text in [
        "",
        "1",
        "000000000000001",
        "00000000000000001",
        "0x00000000000001",
        "+000000000000001",
        " 000000000000001",
        "000000000000001 ",
        "000000000000000g",
        "é00000000000000",
        "../Music/track.m4a",
    ] {
        assert_eq!(
            text.parse::<TrackId>(),
            Err(ParseTrackIdError::InvalidFormat),
            "{text:?}"
        );
    }
    assert_eq!(
        "0000000000000000".parse::<TrackId>(),
        Err(ParseTrackIdError::Zero)
    );
    assert_eq!(TrackId::try_from(0), Err(ParseTrackIdError::Zero));
}

#[test]
fn deletion_api_exposes_explicit_tracks_owned_prune_missing_sources_and_clear() {
    // This external match intentionally has no catch-all arm. Adding a dormant
    // or unqualified operation to the supported engine API must fail this check.
    fn frontend_scope(mode: &DeletionMode) -> (&'static str, usize) {
        match mode {
            DeletionMode::Delete(ids) => ("explicit_tracks", ids.len()),
            DeletionMode::Prune { source, .. } => {
                ("owned_prune", usize::from(!source.as_os_str().is_empty()))
            }
            DeletionMode::Missing { selection, .. } => {
                ("owned_missing_sources", selection.include.len())
            }
            DeletionMode::Clear => ("every_track", 0),
        }
    }
    assert_eq!(frontend_scope(&DeletionMode::Clear), ("every_track", 0));
    assert_eq!(
        frontend_scope(&DeletionMode::Delete(vec![
            "0000000000000001".parse().unwrap()
        ])),
        ("explicit_tracks", 1)
    );
    assert_eq!(
        frontend_scope(&DeletionMode::Prune {
            source: "local-source".into(),
            recursive: false,
        }),
        ("owned_prune", 1)
    );
    assert_eq!(
        frontend_scope(&DeletionMode::Missing {
            source: "local-source".into(),
            selection: SelectionOptions::default(),
        }),
        ("owned_missing_sources", 0)
    );
}

#[tokio::test]
async fn caller_cancellation_and_deadline_refuse_before_state_or_device_work() {
    let root = std::env::temp_dir().join(format!(
        "music-engine-seam-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let source = root.join("source");
    let snapshot = root.join("snapshot");
    let journal = root.join("journal");
    let plan = root.join("plan.json");
    let trace = root.join("trace");
    let cancelled = Arc::new(AtomicBool::new(true));
    let observed_device = Arc::new(AtomicBool::new(false));
    let observer = Arc::clone(&observed_device);
    let engine = Engine::new(
        Config {
            device: DeviceConfig::new(
                Some("EXPLICIT-DEVICE".into()),
                Some("/tmp/unused-engine-socket".into()),
            )
            .unwrap(),
            deadline: None,
            cancelled: Arc::clone(&cancelled),
        },
        move |event| {
            if matches!(event, Event::Device(_)) {
                observer.store(true, Ordering::Relaxed);
            }
            Ok(())
        },
    );
    let selection = SelectionOptions::default();
    let error = engine
        .sync(SyncRequest {
            source: &source,
            state: &root,
            snapshot: Some(&snapshot),
            journal: Some(&journal),
            selection: &selection,
            host_name: "offline-engine-test",
            dry_run: false,
            seconds: 5,
            add_only: false,
            flac_to_alac: false,
            batch_size: 1,
            pinned: None,
            library: None,
            removals: &[],
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    let error = engine
        .apply_change(
            ChangeApplyRequest {
                plan: &plan,
                snapshot: &snapshot,
                journal: &journal,
                trace: &trace,
                state: &journal,
                host_name: "offline-engine-test",
                seconds: 1,
                batch_size: 1,
                check_only: true,
                dry_run: false,
            },
            &mut |_| Ok(true),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    let error = engine
        .cover(CoverRequest {
            file: &source,
            snapshot: &snapshot,
            journal: &journal,
            trace: &trace,
            host_name: "offline-engine-test",
            dry_run: true,
            expected_source: None,
            preserve_album: None,
            pending_cover_only: false,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(
        engine
            .plan(PlanRequest {
                source: &source,
                snapshot: &snapshot,
                journal: None,
                output: Some(&plan),
                selection: &selection,
                add_only: false,
                flac_to_alac: false
            })
            .err()
            .unwrap()
            .to_string()
            .contains("cancelled")
    );
    let error = engine
        .verify(VerificationRequest {
            snapshot: &snapshot,
            journal: &journal,
            trace: &trace,
            tracks: Vec::new(),
            seconds: 1,
        })
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("cancelled"));
    assert!(!root.exists());
    assert!(!observed_device.load(Ordering::Relaxed));

    cancelled.store(false, Ordering::Relaxed);
    let expired = Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: Some(tokio::time::Instant::now() - Duration::from_secs(1)),
        cancelled,
    });
    let error = expired
        .sync(SyncRequest {
            source: &source,
            state: &root,
            snapshot: Some(&snapshot),
            journal: Some(&journal),
            selection: &selection,
            host_name: "offline-engine-test",
            dry_run: true,
            seconds: 5,
            add_only: false,
            flac_to_alac: false,
            batch_size: 1,
            pinned: None,
            library: None,
            removals: &[],
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deadline expired"));
    assert!(!root.exists());

    // Also enter each concrete workflow on the default test stack. This
    // catches oversized future construction that cancellation could bypass.
    assert!(
        engine
            .sync(SyncRequest {
                source: &source,
                state: &root,
                snapshot: Some(&snapshot),
                journal: Some(&journal),
                selection: &selection,
                host_name: "offline-engine-test",
                dry_run: true,
                seconds: 1,
                add_only: false,
                flac_to_alac: false,
                batch_size: 1,
                pinned: None,
                library: None,
                removals: &[],
            })
            .await
            .is_err()
    );
    assert!(
        engine
            .apply_change(
                ChangeApplyRequest {
                    plan: &plan,
                    snapshot: &snapshot,
                    journal: &journal,
                    trace: &trace,
                    state: &journal,
                    host_name: "offline-engine-test",
                    seconds: 1,
                    batch_size: 1,
                    check_only: true,
                    dry_run: false,
                },
                &mut |_| Ok(true),
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .cover(CoverRequest {
                file: &source,
                snapshot: &snapshot,
                journal: &journal,
                trace: &trace,
                host_name: "offline-engine-test",
                dry_run: true,
                expected_source: None,
                preserve_album: None,
                pending_cover_only: false,
            })
            .await
            .is_err()
    );
    assert!(
        engine
            .verify(VerificationRequest {
                snapshot: &snapshot,
                journal: &journal,
                trace: &trace,
                tracks: Vec::new(),
                seconds: 1,
            })
            .await
            .is_err()
    );
    let error = engine
        .delete(
            DeletionRequest {
                dry_run: true,
                snapshot: Some(snapshot.clone()),
                journal: Some(journal.clone()),
                state_dir: root.clone(),
                output: None,
                host_name: "offline-engine-test".into(),
                seconds: 1,
                batch_size: 100,
                leave_for_sync: false,
                pinned: None,
            },
            DeletionMode::Delete(vec![
                "000000000000000a".parse().unwrap(),
                "000000000000000A".parse().unwrap(),
            ]),
            &mut |_| panic!("duplicate track ID must fail before confirmation"),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("zero or duplicate track ID"),
        "{error:#}"
    );
    assert!(
        engine
            .recover(RecoveryRequest {
                snapshot: &snapshot,
                journal: &journal,
                run_dir: None,
                trace: &trace,
                cancel_prepared: None,
                selected_device: None,
            })
            .await
            .is_err()
    );
    assert!(!root.exists());
    assert!(!observed_device.load(Ordering::Relaxed));
}

#[test]
fn frontend_observer_error_stops_planning_before_opening_saved_inputs() {
    let engine = Engine::new(
        Config {
            device: DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        },
        |event| {
            if matches!(event, Event::ReadingSavedSnapshot) {
                anyhow::bail!("frontend output failed");
            }
            Ok(())
        },
    );
    let missing = std::path::Path::new("unopened-engine-input");
    let error = engine
        .plan(PlanRequest {
            source: missing,
            snapshot: missing,
            journal: None,
            output: None,
            selection: &SelectionOptions::default(),
            add_only: false,
            flac_to_alac: false,
        })
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "frontend output failed");
}
