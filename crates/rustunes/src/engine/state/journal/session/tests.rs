//! Sessions on real temporary journals: both halves through every outcome,
//! a Partial insert half compensated afterwards, and the refusals that keep
//! the halves paired and in step.
use super::*;
use crate::engine::state::journal::test_support::{Temp, alac_content, identity};
use coordination::{BatchState, RemovalTarget};
use insertion_batch::{InsertTarget, TargetOutcome};

const OPERATION: [u8; 40] = [7; 40];

fn created(t: &Temp) -> Journal {
    Journal::create(&t.path(), &identity()).unwrap()
}
fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("the call must fail"),
        Err(error) => format!("{error:#}"),
    }
}
fn sql_error(j: &Journal, sql: &str) -> String {
    j.connection.execute_batch("SAVEPOINT probe").unwrap();
    let result = j.connection.execute_batch(sql);
    j.connection
        .execute_batch("ROLLBACK TO probe; RELEASE probe")
        .unwrap();
    format!(
        "{:#}",
        result.expect_err(&format!("statement must fail: {sql}"))
    )
}
fn idle(j: &Journal) -> bool {
    j.outstanding_insert_batch().unwrap().is_none()
        && j.outstanding_removal_batch().unwrap().is_none()
}
fn claim(content: &Content, anchor: u32) -> CompletionEvidence {
    CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: anchor,
        receipt_sha256: [9; 32],
    }
}
fn insert_intent(seeds: &[u8], operation: &[u8], base_anchor: u32) -> InsertIntent {
    InsertIntent {
        coordination: None,
        operation: operation.to_vec(),
        base_anchor,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/session-fixture".into(),
        targets: seeds
            .iter()
            .map(|s| InsertTarget {
                content: alac_content(*s),
                new_album_pid: None,
                new_artist_pid: None,
                conversion: None,
                source: None,
            })
            .collect(),
    }
}
/// Tracks `seeds` inserted and bound by an ordinary batch at anchor 13.
fn bound(j: &mut Journal, seeds: &[u8]) {
    let id = j
        .prepare_insert_batch(&insert_intent(seeds, &[5; 40], 13))
        .unwrap();
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let claims: Vec<_> = seeds
        .iter()
        .enumerate()
        .map(|(n, s)| (n, claim(&alac_content(*s), 14)))
        .collect();
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &claims, &[])
            .unwrap(),
        InsertBatchState::Verified
    );
}
fn owned(j: &Journal, seed: u8) -> RemovalTarget {
    let content = alac_content(seed);
    let active = j.lookup_active(&content.source.audio).unwrap().unwrap();
    RemovalTarget {
        track: content.track_pid,
        item_pid: content.track_pid as i64,
        path: Some(content.path.clone()),
        file_bytes: Some(content.source.file_bytes),
        file_sha256: Some(content.source.full_file_sha256),
        binding: Some((active.reference.id, active.reference.version)),
    }
}
fn removal_intent(targets: Vec<RemovalTarget>) -> RemovalIntent {
    RemovalIntent {
        coordination: None,
        compensates: None,
        compensates_insert_batch: None,
        operation: OPERATION.to_vec(),
        base_anchor: 14,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/session-fixture".into(),
        targets,
        playlists: Vec::new(),
    }
}
/// A session at anchor 14 removing the bound tracks `removed` and inserting
/// `inserted`.
fn prepared(j: &mut Journal, removed: &[u8], inserted: &[u8]) -> SessionIds {
    let targets = removed.iter().map(|s| owned(j, *s)).collect();
    j.prepare_session(
        &insert_intent(inserted, &OPERATION, 14),
        &removal_intent(targets),
    )
    .unwrap()
}

#[test]
fn a_session_retires_its_removals_and_binds_its_insertions_together() {
    let t = Temp::new();
    let mut j = created(&t);
    bound(&mut j, &[4, 5]);
    let ids = prepared(&mut j, &[4], &[7, 8]);
    let removal = j.removal_batch(ids.removal).unwrap();
    assert_eq!(removal.with_insert_batch, Some(ids.insert));
    assert_eq!(removal.state, BatchState::Prepared);
    assert_eq!(j.session_removal(ids.insert).unwrap(), Some(removal));
    assert_eq!(
        j.outstanding_insert_batch().unwrap().unwrap().id,
        ids.insert
    );
    // The session owns the journal.
    assert!(
        failure(j.prepare_insert_batch(&insert_intent(&[9], &[5; 40], 14))).contains("outstanding")
    );
    assert!(
        failure(j.prepare_removal_batch(&removal_intent(vec![owned(&j, 5)])))
            .contains("outstanding")
    );
    // The halves start together with one receipt and signature.
    let permit = j.start_session(ids, [8; 32], [1; 21]).unwrap();
    assert_eq!(permit.operation(), &OPERATION);
    let removal = j.removal_batch(ids.removal).unwrap();
    assert_eq!(
        (removal.state, removal.ready_receipt, removal.signature),
        (BatchState::Started, Some([8; 32]), Some([1; 21]))
    );
    assert_eq!(
        j.insert_batch(ids.insert).unwrap().state,
        InsertBatchState::Started
    );
    // A reopened journal holds both Started halves.
    drop(j);
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    let claims = [
        (0, claim(&alac_content(7), 15)),
        (1, claim(&alac_content(8), 15)),
    ];
    assert_eq!(
        j.settle_session(ids, [6; 32], [7; 32], 15, &claims, &[])
            .unwrap(),
        InsertBatchState::Verified
    );
    assert!(idle(&j));
    assert_eq!(
        j.removal_batch(ids.removal).unwrap().state,
        BatchState::Verified
    );
    assert!(
        j.lookup_active(&alac_content(4).source.audio)
            .unwrap()
            .is_none()
    );
    for seed in [5, 7, 8] {
        assert!(
            j.lookup_active(&alac_content(seed).source.audio)
                .unwrap()
                .is_some()
        );
    }
    drop(j);
    Journal::open_read_only(&t.path(), &identity()).unwrap();
}

#[test]
fn a_session_is_cancelled_or_not_applied_as_one() {
    let t = Temp::new();
    let mut j = created(&t);
    bound(&mut j, &[4]);
    let ids = prepared(&mut j, &[4], &[7]);
    j.cancel_session(ids).unwrap();
    assert!(idle(&j));
    assert_eq!(
        j.removal_batch(ids.removal).unwrap().state,
        BatchState::Cancelled
    );
    assert_eq!(
        j.insert_batch(ids.insert).unwrap().state,
        InsertBatchState::Cancelled
    );
    // A cancelled session's insertions stay reserved; another session takes
    // the removal and a new insertion.
    let ids = prepared(&mut j, &[4], &[8]);
    j.start_session(ids, [8; 32], [1; 21]).unwrap();
    assert!(failure(j.cancel_session(ids)).contains("only a Prepared"));
    j.mark_session_not_applied(ids, [6; 32]).unwrap();
    assert!(idle(&j));
    assert_eq!(
        j.removal_batch(ids.removal).unwrap().state,
        BatchState::NotApplied
    );
    assert_eq!(
        j.insert_batch(ids.insert).unwrap().state,
        InsertBatchState::NotApplied
    );
    assert!(
        j.lookup_active(&alac_content(4).source.audio)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_partial_insert_half_is_compensated_after_its_session() {
    let t = Temp::new();
    let mut j = created(&t);
    bound(&mut j, &[4]);
    let ids = prepared(&mut j, &[4], &[7, 8]);
    j.start_session(ids, [8; 32], [1; 21]).unwrap();
    let claims = [(0, claim(&alac_content(7), 15))];
    assert_eq!(
        j.settle_session(ids, [6; 32], [7; 32], 15, &claims, &[])
            .unwrap(),
        InsertBatchState::Partial
    );
    assert_eq!(
        j.removal_batch(ids.removal).unwrap().state,
        BatchState::Verified
    );
    let mut compensation = removal_intent(vec![RemovalTarget {
        track: alac_content(8).track_pid,
        item_pid: 99,
        path: None,
        file_bytes: None,
        file_sha256: None,
        binding: None,
    }]);
    compensation.compensates_insert_batch = Some(ids.insert);
    compensation.operation = vec![6; 40];
    compensation.base_anchor = 15;
    let batch = j.prepare_removal_batch(&compensation).unwrap();
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(batch, [5; 32], 16).unwrap();
    let insert = j.insert_batch(ids.insert).unwrap();
    assert_eq!(insert.state, InsertBatchState::Compensated);
    assert_eq!(insert.targets[1].outcome, Some(TargetOutcome::Compensated));
    assert!(idle(&j));
}

#[test]
fn the_halves_must_match_and_move_together() {
    let t = Temp::new();
    let mut j = created(&t);
    bound(&mut j, &[4]);
    // Halves that disagree, or a removal half that does more than remove
    // tracks, are refused before anything is written.
    let insert = insert_intent(&[7], &OPERATION, 14);
    let mut other = removal_intent(vec![owned(&j, 4)]);
    other.operation = vec![6; 40];
    assert!(failure(j.prepare_session(&insert, &other)).contains("share one operation"));
    let mut other = removal_intent(vec![owned(&j, 4)]);
    other.evidence = "/run/elsewhere".into();
    assert!(failure(j.prepare_session(&insert, &other)).contains("share one operation"));
    let mut other = removal_intent(Vec::new());
    other.playlists = vec![coordination::PlaylistTarget {
        container: 3,
        name: "Fixture".into(),
    }];
    assert!(
        failure(j.prepare_session(&insert, &other)).contains("removes tracks and nothing else")
    );
    assert!(idle(&j));
    let ids = prepared(&mut j, &[4], &[7]);
    // SQL holds the pairing and the order too.
    assert!(
        sql_error(&j, "UPDATE removal_batches SET state=1,ready_receipt=zeroblob(32),signature=zeroblob(21) WHERE with_insert_batch IS NOT NULL")
            .contains("starts with its insert half")
    );
    assert!(
        sql_error(
            &j,
            &format!(
                "UPDATE removal_batches SET with_insert_batch=NULL WHERE id={}",
                ids.removal.as_i64()
            )
        )
        .contains("immutable")
    );
    j.start_session(ids, [8; 32], [1; 21]).unwrap();
    assert!(
        sql_error(
            &j,
            &format!(
                "UPDATE insert_batches SET state=4,result_receipt=zeroblob(32) WHERE id={}",
                ids.insert.as_i64()
            )
        )
        .contains("after its removal half")
    );
    // A half settled alone leaves the session split and is refused.
    let claims = [(0, claim(&alac_content(7), 15))];
    assert!(
        failure(j.settle_insert_batch(ids.insert, [7; 32], 15, &claims, &[]))
            .contains("after its removal half")
    );
    assert!(failure(j.settle_removal_batch(ids.removal, [6; 32], 15)).contains("different states"));
    assert_eq!(
        j.removal_batch(ids.removal).unwrap().state,
        BatchState::Started
    );
    j.settle_session(ids, [6; 32], [7; 32], 15, &claims, &[])
        .unwrap();
    // A removal half must name a Prepared insert half with the same evidence.
    assert!(
        sql_error(&j, &format!("INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence,with_insert_batch) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x',{})", ids.insert.as_i64()))
            .contains("pairs with its Prepared insert half")
    );
}
