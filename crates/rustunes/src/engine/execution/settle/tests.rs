use super::*;
use crate::engine::state::journal::test_support::{
    Temp, alac_content, identity, prepare_insert, prepare_insert_with_input,
};
use crate::engine::state::journal::{
    AttemptState, InsertBatchState, InsertIntent, InsertTarget, StartEvidence,
};
use serde_json::json;
use std::sync::{Arc, atomic::AtomicBool};

fn quiet(cancelled: bool) -> Engine {
    Engine::quiet(crate::engine::Config {
        device: crate::engine::DeviceConfig::default(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(cancelled)),
    })
}

/// The journal in `t`, with `t/state` as the state directory and
/// `/run/snap` as this run's capture.
fn earlier(t: &Temp) -> Earlier<'static> {
    let journal: &'static Path = Box::leak(t.path().into_boxed_path());
    let state: &'static Path = Box::leak(t.0.join("state").into_boxed_path());
    Earlier {
        journal,
        identity: Box::leak(Box::new(identity())),
        state,
        snapshot: Path::new("/run/snap"),
    }
}

fn started() -> StartEvidence {
    StartEvidence {
        ready_media_anchor: 13,
        ready_receipt_sha256: [8; 32],
        signature: [1; 21],
    }
}

/// A Prepared batch an earlier run never sent is cancelled before the
/// next selection, without connecting; an idle or missing journal has
/// nothing to settle. A run tries a batch again only when nothing is
/// left outstanding and it was not cancelled.
#[tokio::test]
async fn an_earlier_prepared_batch_is_cancelled_before_selecting() {
    use crate::engine::state::journal::test_support::{Temp, identity};
    use crate::engine::state::journal::{PlaylistTarget, RemovalIntent};
    let engine = quiet(false);
    let t = Temp::new();
    assert_eq!(
        earlier(&t)
            .settle(&t.0.join("settle"), "test", &engine, 1)
            .await
            .unwrap(),
        None
    );
    let mut journal = Journal::create(&t.path(), &identity()).unwrap();
    let id = journal
        .prepare_removal_batch(&RemovalIntent {
            coordination: None,
            compensates: None,
            compensates_insert_batch: None,
            operation: vec![5; 40],
            base_anchor: 14,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/unsent".into(),
            targets: Vec::new(),
            playlists: vec![PlaylistTarget {
                container: 3,
                name: "CLI Removal Test 3".into(),
            }],
        })
        .unwrap();
    drop(journal);
    // A dry run settles nothing: it names both ways to settle the batch.
    let error = earlier(&t).refuse(" --wifi").unwrap_err().to_string();
    assert!(
        error.starts_with(&format!(
            "removal batch {} from an earlier run is unfinished; the same command without --dry-run settles it first, or rustunes resume --snapshot /run/snap --journal ",
            id.as_i64()
        )) && error.contains(" --trace-dir NEW_DIRECTORY --wifi settles it alone"),
        "{error}"
    );
    // The deadline must outlast the test, or the control reads as stopped.
    let control = engine.control(600);
    assert!(!removal_batch::may_try_again(&t.path(), &identity(), &control).unwrap());
    assert_eq!(
        earlier(&t)
            .settle(&t.0.join("settle"), "test", &engine, 1)
            .await
            .unwrap(),
        Some(BTreeSet::new())
    );
    assert!(removal_batch::may_try_again(&t.path(), &identity(), &control).unwrap());
    let stopped = quiet(true);
    assert!(!removal_batch::may_try_again(&t.path(), &identity(), &stopped.control(600)).unwrap());
    let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(
        journal.removal_batch(id).unwrap().state,
        BatchState::Cancelled
    );
    assert!(journal.outstanding_removal_batch().unwrap().is_none());
    drop(journal);
    earlier(&t).refuse("").unwrap();
    assert_eq!(
        earlier(&t)
            .settle(&t.0.join("settle"), "test", &engine, 1)
            .await
            .unwrap(),
        None
    );
}

/// A Prepared session an earlier run never sent is cancelled, both
/// halves, through its insert half, without connecting.
#[tokio::test]
async fn an_earlier_prepared_session_is_cancelled_with_both_halves() {
    use crate::engine::state::journal::test_support::{Temp, alac_content, identity};
    use crate::engine::state::journal::{
        InsertBatchState, InsertIntent, InsertTarget, RemovalIntent, RemovalTarget,
    };
    let engine = quiet(false);
    let t = Temp::new();
    let evidence = t.0.join("session");
    std::fs::create_dir(&evidence).unwrap();
    let input = serde_json::to_vec(&json!({"members":[{
        "source_path":"/library/gone.m4a","album":"0000000000000011","artist":"0000000000000012",
    }]}))
    .unwrap();
    std::fs::write(evidence.join("input.json"), &input).unwrap();
    let mut journal = Journal::create(&t.path(), &identity()).unwrap();
    let ids = journal
        .prepare_session(
            &InsertIntent {
                coordination: None,
                operation: vec![5; 40],
                base_anchor: 14,
                before_current: [1; 32],
                before_index: [2; 32],
                input_receipt: removal_batch::sha(&input),
                evidence: evidence.clone(),
                targets: vec![InsertTarget {
                    content: alac_content(1),
                    new_album_pid: None,
                    new_artist_pid: None,
                    conversion: None,
                    source: None,
                }],
            },
            &RemovalIntent {
                coordination: None,
                compensates: None,
                compensates_insert_batch: None,
                operation: vec![5; 40],
                base_anchor: 14,
                before_current: [1; 32],
                before_index: [2; 32],
                input_receipt: removal_batch::sha(&input),
                evidence,
                targets: vec![RemovalTarget {
                    track: 7,
                    item_pid: 7,
                    path: None,
                    file_bytes: None,
                    file_sha256: None,
                    binding: None,
                }],
                playlists: Vec::new(),
            },
        )
        .unwrap();
    drop(journal);
    assert_eq!(
        earlier(&t)
            .settle(&t.0.join("settle"), "test", &engine, 1)
            .await
            .unwrap(),
        Some(BTreeSet::new())
    );
    let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(
        journal.removal_batch(ids.removal).unwrap().state,
        BatchState::Cancelled
    );
    assert_eq!(
        journal.insert_batch(ids.insert).unwrap().state,
        InsertBatchState::Cancelled
    );
    assert!(journal.outstanding_insert_batch().unwrap().is_none());
}

/// A row may lack audio only as a target of the unfinished insertion: an
/// insert batch, or a single insert.
#[test]
fn rows_without_audio_belong_to_the_unfinished_insertion() {
    use crate::engine::state::journal::test_support::{Temp, alac_content, identity};
    use crate::engine::state::journal::{InsertIntent, InsertTarget};
    let t = Temp::new();
    let target = alac_content(1).track_pid;
    let rows = |tracks: &[u64]| tracks.iter().copied().collect::<BTreeSet<_>>();
    earlier(&t).only_unfinished(&rows(&[])).unwrap();
    assert!(earlier(&t).only_unfinished(&rows(&[target])).is_err());
    let mut journal = Journal::create(&t.path(), &identity()).unwrap();
    let batch = journal
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: vec![5; 40],
            base_anchor: 14,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: [3; 32],
            evidence: "/run/batch".into(),
            targets: vec![InsertTarget {
                content: alac_content(1),
                new_album_pid: None,
                new_artist_pid: None,
                conversion: None,
                source: None,
            }],
        })
        .unwrap();
    journal.start_insert_batch(batch, [8; 32], [1; 21]).unwrap();
    drop(journal);
    earlier(&t).only_unfinished(&rows(&[target])).unwrap();
    let error = earlier(&t)
        .only_unfinished(&rows(&[target, 99]))
        .err()
        .expect("a row no batch explains was accepted");
    assert!(error.to_string().contains("0000000000000063"), "{error:#}");
}

/// An insert batch an earlier sync prepared and never sent is cancelled on
/// its own, without connecting: the next sync then plans from fresh state.
#[tokio::test]
async fn an_earlier_prepared_insert_batch_is_cancelled_alone() {
    let engine = quiet(false);
    let t = Temp::new();
    let evidence = t.0.join("batch");
    std::fs::create_dir(&evidence).unwrap();
    let input = serde_json::to_vec(&json!({"members":[{
        "source_path":"/library/new.m4a","album":"0000000000000011","artist":"0000000000000012",
    }]}))
    .unwrap();
    std::fs::write(evidence.join("input.json"), &input).unwrap();
    let mut journal = Journal::create(&t.path(), &identity()).unwrap();
    let batch = journal
        .prepare_insert_batch(&InsertIntent {
            coordination: None,
            operation: vec![5; 40],
            base_anchor: 14,
            before_current: [1; 32],
            before_index: [2; 32],
            input_receipt: removal_batch::sha(&input),
            evidence,
            targets: vec![InsertTarget {
                content: alac_content(1),
                new_album_pid: None,
                new_artist_pid: None,
                conversion: None,
                source: None,
            }],
        })
        .unwrap();
    drop(journal);
    let error = earlier(&t).refuse("").unwrap_err().to_string();
    assert!(
        error.starts_with(&format!(
            "insert batch {} from an earlier run is unfinished; the same command without --dry-run settles it first",
            batch.as_i64()
        )),
        "{error}"
    );
    assert_eq!(
        earlier(&t)
            .settle(&t.0.join("settle"), "test", &engine, 1)
            .await
            .unwrap(),
        Some(BTreeSet::new())
    );
    let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(
        journal.insert_batch(batch).unwrap().state,
        InsertBatchState::Cancelled
    );
    drop(journal);
    earlier(&t).refuse("").unwrap();
}

/// A single insert an earlier run prepared and never started sent nothing:
/// it is cancelled, with the trace `resume --cancel-prepared` writes. A
/// Started one is observed with the run that started it; when no run under
/// the state directory holds its input, the refusal names the `resume`.
#[tokio::test]
async fn an_earlier_single_insert_is_cancelled_or_needs_the_run_that_started_it() {
    let engine = quiet(false);
    let t = Temp::new();
    let mut journal = Journal::create(&t.path(), &identity()).unwrap();
    let id = prepare_insert(&mut journal, alac_content(1));
    drop(journal);
    let error = earlier(&t).refuse(" --wifi").unwrap_err().to_string();
    assert!(
        error.starts_with(&format!(
            "attempt {} from an earlier run is unfinished; the same command without --dry-run settles it first, or rustunes resume --snapshot /run/snap --journal ",
            id.as_i64()
        )) && error.contains(&format!(
            " --trace-dir NEW_DIRECTORY --cancel-prepared {} --wifi settles it alone",
            id.as_i64()
        )),
        "{error}"
    );
    let trace = t.0.join("settle");
    assert_eq!(
        earlier(&t)
            .settle(&trace, "test", &engine, 1)
            .await
            .unwrap(),
        Some(BTreeSet::new())
    );
    assert!(trace.join("cancel-intent.json").is_file());
    let journal = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Cancelled);
    assert!(journal.outstanding_attempt().unwrap().is_none());
    drop(journal);
    earlier(&t).refuse("").unwrap();
    // Started, and no run holds its input.
    let mut journal = Journal::open(&t.path(), &identity()).unwrap();
    let id = prepare_insert_with_input(&mut journal, alac_content(2), [9; 32]);
    drop(journal.mark_started(id, started()).unwrap());
    drop(journal);
    let error = earlier(&t)
        .settle(&t.0.join("settle-2"), "test", &engine, 1)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!(
            "attempt {} was started and not finished",
            id.as_i64()
        )) && error.contains("--run-dir RUN --trace-dir NEW_DIRECTORY, where RUN is the run"),
        "{error}"
    );
    let error = earlier(&t).refuse("").unwrap_err().to_string();
    assert!(
        error.starts_with(&format!(
            "attempt {} from an earlier run is unfinished; the same command without --dry-run settles it first, or rustunes resume --snapshot SNAPSHOT",
            id.as_i64()
        )),
        "{error}"
    );
    // The row it may have left without audio is its own.
    earlier(&t)
        .only_unfinished(&BTreeSet::from([alac_content(2).track_pid]))
        .unwrap();
    assert!(
        earlier(&t)
            .only_unfinished(&BTreeSet::from([alac_content(1).track_pid]))
            .is_err()
    );
}
