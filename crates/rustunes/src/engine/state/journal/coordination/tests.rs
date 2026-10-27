//! Coordinations, removal batches and reserved identities on real temporary
//! journals, and the created journal against the reference DDL.
use super::*;
use crate::engine::state::journal::test_support::{
    Temp, alac_content, identity, insert_started, insert_verified, prepare_insert,
    try_prepare_insert,
};
use std::sync::MutexGuard;

/// Permits are process-wide; tests that enter one run one at a time.
static PERMITS: Mutex<()> = Mutex::new(());
fn permits() -> MutexGuard<'static, ()> {
    PERMITS.lock().unwrap_or_else(|e| e.into_inner())
}

fn created(t: &Temp) -> Journal {
    Journal::create(&t.path(), &identity()).unwrap()
}
fn definitions(c: &Connection) -> Vec<(String, String)> {
    c.prepare("SELECT name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("the call must fail"),
        Err(error) => format!("{error:#}"),
    }
}
/// The error of a statement that must fail; nothing it did survives.
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
fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn intent(targets: Vec<RemovalTarget>) -> RemovalIntent {
    RemovalIntent {
        coordination: None,
        compensates: None,
        compensates_insert_batch: None,
        operation: vec![5; 40],
        base_anchor: 13,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/removal-fixture".into(),
        targets,
        playlists: Vec::new(),
    }
}
/// The owned active binding of `content`, named as a removal target.
fn owned(j: &Journal, content: &Content) -> RemovalTarget {
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
/// A catalogue row with a file but no binding of ours.
fn unowned(track: u64, path: &str) -> RemovalTarget {
    RemovalTarget {
        track,
        item_pid: track as i64,
        path: Some(path.into()),
        file_bytes: Some(900),
        file_sha256: Some([55; 32]),
        binding: None,
    }
}
/// A catalogue row whose file never arrived.
fn locationless(track: u64) -> RemovalTarget {
    RemovalTarget {
        track,
        item_pid: track as i64,
        path: None,
        file_bytes: None,
        file_sha256: None,
        binding: None,
    }
}
fn with_bindings(t: &Temp) -> (Journal, Content, Content) {
    let mut j = created(t);
    let (a, b) = (alac_content(1), alac_content(2));
    insert_verified(&mut j, a.clone());
    insert_verified(&mut j, b.clone());
    (j, a, b)
}
fn tombstones(j: &Journal) -> Vec<(i64, u32, i64, Vec<u8>, u32)> {
    j.connection
        .prepare("SELECT binding_id,binding_version,batch,verified_receipt,verified_anchor FROM binding_tombstones ORDER BY binding_id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// --- reference DDL and creation -------------------------------------------

#[test]
fn created_journal_validates_reopens_and_matches_the_reference() {
    let t = Temp::new();
    let j = created(&t);
    let reference = Connection::open_in_memory().unwrap();
    reference.execute_batch(SCHEMA).unwrap();
    assert_eq!(definitions(&j.connection), definitions(&reference));
    let names: BTreeSet<String> = definitions(&j.connection)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for name in [
        "coordinations",
        "coordination_steps",
        "removal_batches",
        "removal_targets",
        "one_active_coordination",
        "one_outstanding_batch",
        "one_insert_compensation_batch",
        "binding_tombstones",
        "compensated_parent",
    ] {
        assert!(names.contains(name), "{name}");
    }
    assert!(j.route_owner().unwrap().is_none());
    drop(j);
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert!(read.outstanding().unwrap().is_none());
    drop(read);
    let mut writer = Journal::open(&t.path(), &identity()).unwrap();
    insert_verified(&mut writer, alac_content(1));
    assert_eq!(writer.active_bindings().unwrap().len(), 1);
    // Reopening again validates the written history.
    drop(writer);
    Journal::open(&t.path(), &identity()).unwrap();
}

// --- removal batches -----------------------------------------------------------

#[test]
fn removal_batch_settles_owned_targets_and_leaves_unowned_and_other_rows_alone() {
    let t = Temp::new();
    let (mut j, a, b) = with_bindings(&t);
    let unowned_path = "iTunes_Control/Music/F40/U001.mp3";
    let request = intent(vec![owned(&j, &a), unowned(4242, unowned_path)]);
    let id = j.prepare_removal_batch(&request).unwrap();
    let prepared = j.removal_batch(id).unwrap();
    assert_eq!(prepared.state, BatchState::Prepared);
    assert_eq!(prepared.targets, request.targets);
    assert_eq!(prepared.operation, request.operation);
    assert!(prepared.ready_receipt.is_none() && prepared.coordination.is_none());
    assert_eq!(j.outstanding_removal_batch().unwrap().unwrap().id, id);
    // Outstanding work blocks every ordinary writer and idleness check.
    let error = failure(try_prepare_insert(&mut j, alac_content(5)));
    assert!(error.contains("outstanding removal batch"), "{error}");
    let error = failure(j.outstanding());
    assert!(error.contains("outstanding removal batch"), "{error}");
    assert!(failure(j.outstanding()).contains("outstanding removal batch"));
    assert!(failure(ensure_idle_of(&j)).contains("outstanding removal batch"));
    let error = sql_error(
        &j,
        "INSERT INTO attempts(kind,state,content,operation,operation_sha,base_anchor,input_receipt) VALUES(1,0,x'01',x'01',zeroblob(32),0,zeroblob(32))",
    );
    assert!(
        error.contains("outstanding removal batch requires settlement"),
        "{error}"
    );
    // A second batch cannot overlap.
    let error = failure(j.prepare_removal_batch(&intent(vec![unowned(
        4243,
        "iTunes_Control/Music/F40/U002.mp3",
    )])));
    assert!(error.contains("outstanding removal batch"), "{error}");
    // Start is durable and single-use.
    let started = j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    assert_eq!(started.operation(), request.operation);
    assert_eq!(started.signature(), &[1; 21]);
    let record = j.removal_batch(id).unwrap();
    assert_eq!(record.state, BatchState::Started);
    assert_eq!(record.ready_receipt, Some([8; 32]));
    assert_eq!(record.signature, Some([1; 21]));
    assert!(failure(j.start_removal_batch(id, [8; 32], [1; 21])).contains("already started"));
    assert!(failure(j.cancel_removal_batch(id)).contains("only a Prepared"));
    assert!(
        failure(try_prepare_insert(&mut j, alac_content(5))).contains("outstanding removal batch")
    );
    // Settlement retires the owned target in the same transaction.
    j.settle_removal_batch(id, [9; 32], 14).unwrap();
    let record = j.removal_batch(id).unwrap();
    assert_eq!(record.state, BatchState::Verified);
    assert_eq!(record.result_receipt, Some([9; 32]));
    assert_eq!(record.verified_anchor, Some(14));
    assert!(j.outstanding_removal_batch().unwrap().is_none());
    assert!(j.lookup_active(&a.source.audio).unwrap().is_none());
    assert!(j.lookup_active(&b.source.audio).unwrap().is_some());
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    let binding = request.targets[0].binding.unwrap();
    assert_eq!(
        tombstones(&j),
        [(binding.0.as_i64(), binding.1, id.as_i64(), vec![9; 32], 14)]
    );
    assert_eq!(j.history_summary().unwrap().retired_binding_count, 1);
    // The unowned target changed no binding history, but its identities stay
    // reserved for good.
    assert!(j.reserved_pids().unwrap().contains(&4242));
    assert!(
        j.reserved_paths()
            .unwrap()
            .contains(&unowned_path.to_ascii_lowercase())
    );
    // The journal is idle again and the history reopens.
    insert_verified(&mut j, alac_content(5));
    drop(j);
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(read.removal_batches().unwrap().len(), 1);
    assert_eq!(read.removal_batch_counts().unwrap().verified, 1);
    drop(read);
    Journal::open(&t.path(), &identity()).unwrap();
}
/// `ensure_idle` over a handle's connection.
fn ensure_idle_of(j: &Journal) -> Result<()> {
    ensure_idle(&j.connection)
}

#[test]
fn removal_batch_cancelled_and_not_applied_leave_every_binding_active() {
    let t = Temp::new();
    let (mut j, a, b) = with_bindings(&t);
    let target = owned(&j, &a);
    // Cancelled: never sent.
    let id = j
        .prepare_removal_batch(&intent(vec![target.clone()]))
        .unwrap();
    j.cancel_removal_batch(id).unwrap();
    assert_eq!(j.removal_batch(id).unwrap().state, BatchState::Cancelled);
    assert!(
        failure(j.start_removal_batch(id, [8; 32], [1; 21])).contains("already started or closed")
    );
    assert!(j.not_applied_removal_tracks().unwrap().is_empty());
    assert!(j.outstanding_removal_batch().unwrap().is_none());
    assert_eq!(j.active_bindings().unwrap().len(), 2);
    assert!(tombstones(&j).is_empty());
    // NotApplied: sent, then proven unapplied.
    let id = j
        .prepare_removal_batch(&intent(vec![target.clone()]))
        .unwrap();
    assert!(failure(j.mark_removal_batch_not_applied(id, [7; 32])).contains("only a Started"));
    j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    j.mark_removal_batch_not_applied(id, [7; 32]).unwrap();
    let record = j.removal_batch(id).unwrap();
    assert_eq!(record.state, BatchState::NotApplied);
    // Its session may have taken the target's file before the rollback.
    assert_eq!(
        j.not_applied_removal_tracks().unwrap(),
        BTreeSet::from([a.track_pid])
    );
    assert_eq!(record.result_receipt, Some([7; 32]));
    assert_eq!(record.verified_anchor, None);
    assert!(failure(j.settle_removal_batch(id, [9; 32], 14)).contains("not Started"));
    assert_eq!(j.active_bindings().unwrap().len(), 2);
    assert!(j.lookup_active(&b.source.audio).unwrap().is_some());
    assert!(tombstones(&j).is_empty());
    // The same row can be targeted again, and then settles.
    let id = j.prepare_removal_batch(&intent(vec![target])).unwrap();
    j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(id, [9; 32], 14).unwrap();
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    assert_eq!(
        j.not_applied_removal_tracks().unwrap(),
        BTreeSet::from([a.track_pid])
    );
    let counts = j.removal_batch_counts().unwrap();
    assert_eq!(
        counts,
        BatchCounts {
            prepared: 0,
            started: 0,
            verified: 1,
            cancelled: 1,
            not_applied: 1
        }
    );
    // Removed identities stay reserved even for cancelled and unapplied tries.
    assert!(j.reserved_pids().unwrap().contains(&a.track_pid));
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn removal_batch_refuses_attempts_stale_bindings_and_malformed_intents() {
    let t = Temp::new();
    let (mut j, a, _b) = with_bindings(&t);
    // An outstanding attempt blocks preparation.
    let started = insert_started(&mut j, alac_content(6));
    let error = failure(j.prepare_removal_batch(&intent(vec![owned(&j, &a)])));
    assert!(error.contains("outstanding attempt"), "{error}");
    let error = sql_error(
        &j,
        "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
    );
    assert!(error.contains("Prepared on an idle journal"), "{error}");
    j.reconcile_completed(
        started,
        CompletionEvidence {
            track_pid: alac_content(6).track_pid,
            path: alac_content(6).path,
            audio_bytes: alac_content(6).source.file_bytes,
            audio_file_sha256: alac_content(6).source.full_file_sha256,
            artwork_sha256: None,
            media_anchor: 14,
            receipt_sha256: [9; 32],
        },
    )
    .unwrap();
    // Targets must be the active version of their binding.
    let mut stale = owned(&j, &a);
    let (id, version) = stale.binding.unwrap();
    stale.binding = Some((id, version + 1));
    let error = failure(j.prepare_removal_batch(&intent(vec![stale])));
    assert!(error.contains("not the active version"), "{error}");
    let mut wrong_row = owned(&j, &a);
    wrong_row.track += 1;
    let error = failure(j.prepare_removal_batch(&intent(vec![wrong_row])));
    assert!(error.contains("differs from its binding"), "{error}");
    // A retired binding is no longer a target.
    let retired = owned(&j, &a);
    let first = j
        .prepare_removal_batch(&intent(vec![retired.clone()]))
        .unwrap();
    j.start_removal_batch(first, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(first, [9; 32], 14).unwrap();
    let error = failure(j.prepare_removal_batch(&intent(vec![retired])));
    assert!(error.contains("not the active version"), "{error}");
    // The trigger holds even without the Rust check.
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x'); INSERT INTO removal_targets VALUES(last_insert_rowid(),0,x'0000000000000001',1,NULL,NULL,NULL,{},{version});",
            id.as_i64()
        ),
    );
    assert!(error.contains("names an active binding version"), "{error}");
    // Malformed intents.
    let good = unowned(4242, "iTunes_Control/Music/F40/U001.mp3");
    let mut variants: Vec<(&str, RemovalIntent)> = Vec::new();
    variants.push(("between one and", intent(vec![])));
    variants.push((
        "lists a track twice",
        intent(vec![good.clone(), good.clone()]),
    ));
    let mut same_path = good.clone();
    same_path.track = 4243;
    same_path.path = Some("iTunes_Control/Music/F40/u001.mp3".into());
    variants.push(("lists a path twice", intent(vec![good.clone(), same_path])));
    let mut no_size = good.clone();
    no_size.file_bytes = None;
    variants.push(("all present or all absent", intent(vec![no_size])));
    let mut bad_path = good.clone();
    bad_path.path = Some("../escape.mp3".into());
    variants.push(("unsafe observed Music path", intent(vec![bad_path])));
    let mut zero = good.clone();
    zero.track = 0;
    variants.push(("nonzero", intent(vec![zero])));
    let mut empty_operation = intent(vec![good.clone()]);
    empty_operation.operation.clear();
    variants.push(("operation exceeds bounds", empty_operation));
    let mut zero_receipt = intent(vec![good.clone()]);
    zero_receipt.input_receipt = [0; 32];
    variants.push(("missing evidence digest", zero_receipt));
    let mut relative = intent(vec![good.clone()]);
    relative.evidence = "relative/evidence".into();
    variants.push(("absolute path", relative));
    let mut exhausted = intent(vec![good.clone()]);
    exhausted.base_anchor = u32::MAX;
    variants.push(("anchor exhausted", exhausted));
    for (message, request) in variants {
        let error = failure(j.prepare_removal_batch(&request));
        assert!(error.contains(message), "{message}: {error}");
    }
    // Nothing above left a batch behind.
    assert_eq!(j.removal_batches().unwrap().len(), 1);
    assert!(j.outstanding_removal_batch().unwrap().is_none());
    // Start evidence is checked too.
    let id = j.prepare_removal_batch(&intent(vec![good])).unwrap();
    assert!(
        failure(j.start_removal_batch(id, [0; 32], [1; 21])).contains("missing evidence digest")
    );
    assert!(failure(j.start_removal_batch(id, [8; 32], [2; 21])).contains("signature envelope"));
    j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    assert!(
        failure(j.settle_removal_batch(id, [9; 32], 13)).contains("does not cover its revision")
    );
    assert!(failure(j.settle_removal_batch(id, [0; 32], 14)).contains("missing evidence digest"));
    j.settle_removal_batch(id, [9; 32], 14).unwrap();
}

#[test]
fn removal_batch_states_are_forward_only_and_immutable_in_sql() {
    let t = Temp::new();
    let (mut j, a, _b) = with_bindings(&t);
    let make = |j: &mut Journal, n: u64, finish: u8| {
        let id = j
            .prepare_removal_batch(&intent(vec![unowned(
                5000 + n,
                &format!("iTunes_Control/Music/F40/S{n:03}.mp3"),
            )]))
            .unwrap();
        if finish >= 1 {
            j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
        }
        match finish {
            0 => {}
            1 => return id,
            2 => j.settle_removal_batch(id, [9; 32], 14).unwrap(),
            _ => j.mark_removal_batch_not_applied(id, [7; 32]).unwrap(),
        }
        id
    };
    let verified = make(&mut j, 1, 2).as_i64();
    let unapplied = make(&mut j, 2, 3).as_i64();
    let cancelled = {
        let id = make(&mut j, 3, 0);
        j.cancel_removal_batch(id).unwrap();
        id.as_i64()
    };
    let prepared_then_started = make(&mut j, 4, 1).as_i64();
    // Started: no backwards, no skipping, no rewriting of what was signed.
    for sql in [
        format!(
            "UPDATE removal_batches SET state=0,ready_receipt=NULL,signature=NULL WHERE id={prepared_then_started}"
        ),
        format!(
            "UPDATE removal_batches SET state=3,ready_receipt=NULL,signature=NULL WHERE id={prepared_then_started}"
        ),
        format!("UPDATE removal_batches SET state=1 WHERE id={prepared_then_started}"),
        format!(
            "UPDATE removal_batches SET signature=zeroblob(21) WHERE id={prepared_then_started}"
        ),
        format!(
            "UPDATE removal_batches SET ready_receipt=zeroblob(32) WHERE id={prepared_then_started}"
        ),
        format!(
            "UPDATE removal_batches SET operation=x'00',operation_sha=zeroblob(32) WHERE id={prepared_then_started}"
        ),
        format!("UPDATE removal_batches SET base_anchor=1 WHERE id={prepared_then_started}"),
        format!("UPDATE removal_batches SET evidence='/other' WHERE id={prepared_then_started}"),
        format!("UPDATE removal_batches SET compensates=1 WHERE id={prepared_then_started}"),
        format!("UPDATE removal_batches SET id=999 WHERE id={prepared_then_started}"),
        format!("DELETE FROM removal_batches WHERE id={prepared_then_started}"),
        format!("UPDATE removal_targets SET item_pid=7 WHERE batch={prepared_then_started}"),
        format!("DELETE FROM removal_targets WHERE batch={prepared_then_started}"),
        format!(
            "INSERT INTO removal_targets VALUES({prepared_then_started},1,x'0000000000000009',9,NULL,NULL,NULL,NULL,NULL)"
        ),
    ] {
        sql_error(&j, &sql);
    }
    // Terminal batches accept no change at all.
    for id in [verified, unapplied, cancelled] {
        for sql in [
            format!("UPDATE removal_batches SET state=1 WHERE id={id}"),
            format!(
                "UPDATE removal_batches SET state=2,result_receipt=zeroblob(32),verified_anchor=14 WHERE id={id}"
            ),
            format!("UPDATE removal_batches SET state=3 WHERE id={id}"),
            format!("UPDATE removal_batches SET result_receipt=zeroblob(32) WHERE id={id}"),
            format!("DELETE FROM removal_batches WHERE id={id}"),
            format!("UPDATE removal_targets SET track=x'0000000000000002' WHERE batch={id}"),
            format!("DELETE FROM removal_targets WHERE batch={id}"),
            format!(
                "INSERT INTO removal_targets VALUES({id},1,x'0000000000000009',9,NULL,NULL,NULL,NULL,NULL)"
            ),
        ] {
            sql_error(&j, &sql);
        }
    }
    // Shapes the CHECK constraints refuse: Started rows carry their evidence,
    // Verified rows their receipt and anchor, NotApplied no anchor.
    sql_error(
        &j,
        &format!("UPDATE removal_batches SET state=2 WHERE id={prepared_then_started}"),
    );
    sql_error(
        &j,
        &format!(
            "UPDATE removal_batches SET state=4,result_receipt=zeroblob(32),verified_anchor=14 WHERE id={prepared_then_started}"
        ),
    );
    // A Prepared batch starts only to Started or Cancelled.
    j.settle_removal_batch(BatchId(prepared_then_started), [9; 32], 14)
        .unwrap();
    let next = make(&mut j, 5, 0);
    sql_error(
        &j,
        &format!(
            "UPDATE removal_batches SET state=2,ready_receipt=zeroblob(32),signature=x'01',result_receipt=zeroblob(32),verified_anchor=14 WHERE id={}",
            next.as_i64()
        ),
    );
    sql_error(
        &j,
        &format!(
            "UPDATE removal_batches SET state=4 WHERE id={}",
            next.as_i64()
        ),
    );
    // Ordinal gaps, a second outstanding batch, a non-Prepared insert.
    sql_error(
        &j,
        &format!(
            "INSERT INTO removal_targets VALUES({},2,x'0000000000000009',9,NULL,NULL,NULL,NULL,NULL)",
            next.as_i64()
        ),
    );
    sql_error(
        &j,
        "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
    );
    j.cancel_removal_batch(next).unwrap();
    sql_error(
        &j,
        "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence,ready_receipt,signature) VALUES(1,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x',zeroblob(32),zeroblob(21))",
    );
    // The owned binding is untouched by all of it.
    assert!(j.lookup_active(&a.source.audio).unwrap().is_some());
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn removal_batch_compensates_a_started_insert_and_settles_it_compensated() {
    let t = Temp::new();
    let mut j = created(&t);
    insert_verified(&mut j, alac_content(1));
    let content = alac_content(2);
    let parent = insert_started(&mut j, content.clone());
    let mut request = intent(vec![locationless(content.track_pid)]);
    request.compensates = Some(parent);
    // Refusals first: a path, two rows, a binding.
    let mut with_path = request.clone();
    with_path.targets = vec![unowned(content.track_pid, &content.path)];
    assert!(failure(j.prepare_removal_batch(&with_path)).contains("exactly one locationless"));
    let mut two = request.clone();
    two.targets.push(locationless(content.track_pid + 1));
    assert!(failure(j.prepare_removal_batch(&two)).contains("exactly one locationless"));
    let mut bound = request.clone();
    bound.targets[0].binding = owned(&j, &alac_content(1)).binding;
    assert!(failure(j.prepare_removal_batch(&bound)).contains("exactly one locationless"));
    // The same shapes are refused by the triggers.
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO removal_batches(compensates,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x'); INSERT INTO removal_targets VALUES(last_insert_rowid(),0,x'0000000000000001',1,'iTunes_Control/Music/F40/U001.mp3',1,zeroblob(32),NULL,NULL);",
            parent.as_i64()
        ),
    );
    assert!(error.contains("locationless unowned"), "{error}");
    // The Started Insert is the one outstanding attempt the batch may follow.
    let id = j.prepare_removal_batch(&request).unwrap();
    let record = j.removal_batch(id).unwrap();
    assert_eq!(record.compensates, Some(parent));
    assert_eq!(record.targets, request.targets);
    assert!(failure(j.outstanding()).contains("outstanding removal batch"));
    let error = failure(try_prepare_insert(&mut j, alac_content(7)));
    assert!(error.contains("outstanding"), "{error}");
    // A second batch for the same parent cannot overlap the first.
    assert!(failure(j.prepare_removal_batch(&request)).contains("outstanding removal batch"));
    // Without a verified batch the parent cannot be Compensated directly.
    assert!(
        sql_error(
            &j,
            &format!("UPDATE attempts SET state=4 WHERE id={}", parent.as_i64())
        )
        .contains("compensated parent requires a verified compensation")
    );
    j.start_removal_batch(id, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(id, [9; 32], 14).unwrap();
    assert_eq!(j.attempt(parent).unwrap().state, AttemptState::Compensated);
    assert!(j.outstanding().unwrap().is_none());
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    assert!(tombstones(&j).is_empty());
    assert!(j.reserved_pids().unwrap().contains(&content.track_pid));
    // Compensated is final: it cannot be compensated again.
    assert!(j.prepare_removal_batch(&request).is_err());
    insert_verified(&mut j, alac_content(8));
    drop(j);
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(read.removal_batch(id).unwrap().state, BatchState::Verified);
    assert_eq!(
        read.attempt(parent).unwrap().state,
        AttemptState::Compensated
    );
    drop(read);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn compensating_batch_needs_a_started_insert() {
    let t = Temp::new();
    let (mut j, a, _b) = with_bindings(&t);
    let verified_insert = j
        .active_bindings()
        .unwrap()
        .into_iter()
        .find(|b| b.binding.content.track_pid == a.track_pid)
        .and_then(|_| {
            j.connection
                .query_row(
                    "SELECT first_attempt FROM binding_records WHERE track=?1",
                    [a.track_pid.to_be_bytes()],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
        })
        .map(|id| j.attempt(AttemptId(id)).map(|x| x.id).unwrap())
        .unwrap();
    let mut request = intent(vec![locationless(9001)]);
    // A Verified Insert, with nothing outstanding.
    request.compensates = Some(verified_insert);
    let error = failure(j.prepare_removal_batch(&request));
    assert!(error.contains("only outstanding attempt"), "{error}");
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO removal_batches(compensates,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
            verified_insert.as_i64()
        ),
    );
    assert!(error.contains("requires a Started Insert"), "{error}");
    // A merely Prepared Insert has not been sent: nothing to compensate.
    let prepared = prepare_insert(&mut j, alac_content(6));
    request.compensates = Some(prepared);
    let error = failure(j.prepare_removal_batch(&request));
    assert!(error.contains("Started"), "{error}");
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO removal_batches(compensates,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
            prepared.as_i64()
        ),
    );
    assert!(error.contains("requires a Started Insert"), "{error}");
    // Cancel the Prepared Insert so the next case starts idle.
    j.cancel_prepared(prepared).unwrap();
    // A cancelled and then unapplied compensation leaves the parent Started
    // and compensable again.
    let parent = insert_started(&mut j, alac_content(7));
    request.compensates = Some(parent);
    request.targets = vec![locationless(alac_content(7).track_pid)];
    let first = j.prepare_removal_batch(&request).unwrap();
    j.cancel_removal_batch(first).unwrap();
    let second = j.prepare_removal_batch(&request).unwrap();
    j.start_removal_batch(second, [8; 32], [1; 21]).unwrap();
    j.mark_removal_batch_not_applied(second, [7; 32]).unwrap();
    assert_eq!(
        j.attempt(parent).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    let third = j.prepare_removal_batch(&request).unwrap();
    j.start_removal_batch(third, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(third, [9; 32], 14).unwrap();
    assert_eq!(j.attempt(parent).unwrap().state, AttemptState::Compensated);
}

// --- coordinations -----------------------------------------------------------------

#[test]
fn coordination_lifecycle_phases_and_single_active() {
    let t = Temp::new();
    let mut j = created(&t);
    insert_verified(&mut j, alac_content(1));
    let evidence = Path::new("/run/coordination-fixture");
    let id = j
        .begin_coordination(CoordinationKind::ReplaceTrack, b"intent", evidence, 14)
        .unwrap();
    let c = j.coordination(id).unwrap();
    assert_eq!(c.kind, CoordinationKind::ReplaceTrack);
    assert_eq!(c.state, CoordinationState::Active);
    assert_eq!((c.phase, c.revision, c.base_anchor), (0, 0, 14));
    assert_eq!(c.intent, b"intent");
    assert_eq!(c.intent_sha256, sha(b"intent"));
    assert_eq!(c.evidence, evidence);
    assert_eq!(c.outcome, None);
    assert_eq!(j.active_coordination().unwrap(), Some(c.clone()));
    assert_eq!(j.coordinations().unwrap(), [c.clone()]);
    assert_eq!(j.route_owner().unwrap(), Some(id.as_i64()));
    // Only one Active coordination.
    let error = failure(j.begin_coordination(CoordinationKind::Clear, b"two", evidence, 14));
    assert!(error.contains("already Active"), "{error}");
    sql_error(
        &j,
        "INSERT INTO coordinations(kind,state,phase,revision,intent,intent_sha,evidence,base_anchor) VALUES(1,0,0,0,x'0101',zeroblob(32),'/x',0)",
    );
    // Progress is a compare-and-swap on the revision and never rewinds.
    assert_eq!(j.advance_coordination(id, 0, 3).unwrap(), 1);
    assert_eq!(j.advance_coordination(id, 1, 3).unwrap(), 2);
    assert!(failure(j.advance_coordination(id, 1, 4)).contains("stale"));
    assert!(failure(j.advance_coordination(id, 2, 2)).contains("cannot move backwards"));
    let error = sql_error(
        &j,
        &format!(
            "UPDATE coordinations SET phase=1,revision=revision+1 WHERE id={}",
            id.as_i64()
        ),
    );
    assert!(error.contains("cannot move backwards"), "{error}");
    let c = j.coordination(id).unwrap();
    assert_eq!((c.phase, c.revision), (3, 2));
    // Intent, kind and evidence are immutable; revisions advance by one only.
    for sql in [
        "UPDATE coordinations SET intent=x'0202'",
        "UPDATE coordinations SET intent_sha=zeroblob(32)",
        "UPDATE coordinations SET kind=1",
        "UPDATE coordinations SET evidence='/other'",
        "UPDATE coordinations SET base_anchor=0",
        "UPDATE coordinations SET id=7",
        "UPDATE coordinations SET revision=revision+2",
        "UPDATE coordinations SET revision=revision",
        "UPDATE coordinations SET state=1,revision=revision+1",
        "DELETE FROM coordinations",
    ] {
        sql_error(&j, sql);
    }
    // Completion records the outcome and releases the journal.
    j.complete_coordination(id, 2, b"done").unwrap();
    let c = j.coordination(id).unwrap();
    assert_eq!(c.state, CoordinationState::Completed);
    assert_eq!((c.revision, c.outcome.as_deref()), (3, Some(&b"done"[..])));
    assert!(j.active_coordination().unwrap().is_none());
    assert!(j.route_owner().unwrap().is_none());
    assert!(failure(j.advance_coordination(id, 3, 4)).contains("stale or closed"));
    assert!(failure(j.complete_coordination(id, 3, b"again")).contains("stale or closed"));
    for sql in [
        "UPDATE coordinations SET phase=9,revision=revision+1",
        "UPDATE coordinations SET outcome=x'0202',revision=revision+1",
        "DELETE FROM coordinations",
    ] {
        let error = sql_error(&j, sql);
        assert!(
            error.contains("immutable") || error.contains("retained"),
            "{error}"
        );
    }
    insert_verified(&mut j, alac_content(2));
    // A second coordination closes with its own outcome.
    let second = j
        .begin_coordination(CoordinationKind::Clear, b"clear", evidence, 14)
        .unwrap();
    assert!(second > id);
    j.complete_coordination(second, 0, b"cleared").unwrap();
    let c = j.coordination(second).unwrap();
    assert_eq!(c.state, CoordinationState::Completed);
    assert_eq!(c.outcome.as_deref(), Some(&b"cleared"[..]));
    assert_eq!(j.coordinations().unwrap().len(), 2);
    // Validation walks all of it on reopen.
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(read.coordinations().unwrap().len(), 2);
    assert!(failure(read.coordination(CoordinationId(77))).contains("not found"));
}

#[test]
fn coordination_needs_an_idle_journal_and_valid_intent() {
    let t = Temp::new();
    let mut j = created(&t);
    assert!(j.active_coordination().unwrap().is_none());
    assert!(j.coordinations().unwrap().is_empty());
    assert!(j.outstanding_removal_batch().unwrap().is_none());
    assert!(j.removal_batches().unwrap().is_empty());
    assert_eq!(j.removal_batch_counts().unwrap(), BatchCounts::default());
    // Intent, evidence and idleness.
    for (intent, evidence, message) in [
        (&b"x"[..], "/run/x", "intent exceeds bounds"),
        (&b"ok"[..], "relative", "absolute path"),
        (&b"ok"[..], "/", "absolute path"),
    ] {
        let error =
            failure(j.begin_coordination(CoordinationKind::Clear, intent, Path::new(evidence), 0));
        assert!(error.contains(message), "{message}: {error}");
    }
    let parent = insert_started(&mut j, alac_content(1));
    let error =
        failure(j.begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 0));
    assert!(error.contains("outstanding attempt"), "{error}");
    j.cancel_prepared(parent).unwrap_err();
    let mut request = intent(vec![locationless(alac_content(1).track_pid)]);
    request.compensates = Some(parent);
    let batch = j.prepare_removal_batch(&request).unwrap();
    j.cancel_removal_batch(batch).unwrap();
    // A read-only handle cannot write coordination storage.
    drop(j);
    let mut read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert!(
        failure(read.begin_coordination(
            CoordinationKind::Clear,
            b"intent",
            Path::new("/run/x"),
            0
        ))
        .contains("read-only")
    );
}

#[test]
fn continuing_a_replacement_cancels_only_its_never_started_attempt() {
    use crate::engine::execution::replace::cancel_unstarted;
    let _guard = permits();
    let t = Temp::new();
    let mut j = created(&t);
    let id = j
        .begin_coordination(
            CoordinationKind::ReplaceTrack,
            b"intent",
            Path::new("/run/coordination-fixture"),
            13,
        )
        .unwrap();
    let permit = j.enter_coordination(id).unwrap();
    let mut worker = Journal::open(&t.path(), &identity()).unwrap();
    // A run stopped between preparing and starting its insert.
    let prepared = prepare_insert(&mut worker, alac_content(2));
    cancel_unstarted(&t.path(), &identity()).unwrap();
    assert_eq!(
        worker.attempt(prepared).unwrap().state,
        AttemptState::Cancelled
    );
    // Started work is left for resume to observe.
    let started = insert_started(&mut worker, alac_content(3));
    cancel_unstarted(&t.path(), &identity()).unwrap();
    assert_eq!(
        worker.attempt(started).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    drop(permit);
    // Another process (no permit) cannot cancel the coordination's work.
    drop(worker);
    let mut other = Journal::open(&t.path(), &identity()).unwrap();
    assert!(failure(other.cancel_prepared(started)).contains("owns the journal"));
}

#[test]
fn coordination_owns_the_journal_until_closed_except_for_its_permit() {
    let _guard = permits();
    let t = Temp::new();
    let mut j = created(&t);
    insert_verified(&mut j, alac_content(1));
    let id = j
        .begin_coordination(
            CoordinationKind::ReplaceLibrary,
            b"reset",
            Path::new("/run/coordination-fixture"),
            14,
        )
        .unwrap();
    // Every ordinary writer refuses while the coordination is Active.
    let error = failure(try_prepare_insert(&mut j, alac_content(2)));
    assert!(error.contains("owns the journal"), "{error}");
    assert!(
        error.contains(&format!("coordination {}", id.as_i64())),
        "{error}"
    );
    assert!(failure(j.ordinary_usable()).contains("owns the journal"));
    let no_permit = j.prepare_removal_batch(&intent(vec![unowned(
        4242,
        "iTunes_Control/Music/F40/U001.mp3",
    )]));
    assert!(failure(no_permit).contains("owns the journal"));
    // Reads and the coordination's own API stay available.
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    j.advance_coordination(id, 0, 1).unwrap();
    // The coordinator's process holds the one permit.
    let other = j.enter_coordination(CoordinationId(id.as_i64() + 5));
    assert!(failure(other).contains("not the Active coordination"));
    let permit = j.enter_coordination(id).unwrap();
    assert!(failure(j.enter_coordination(id)).contains("already held"));
    // It lets the coordinator's own handles run the ordinary internals.
    let mut worker = Journal::open(&t.path(), &identity()).unwrap();
    // An attempt prepared under the permit is the coordination's next step.
    let attempt = prepare_insert(&mut worker, alac_content(2));
    assert_eq!(
        j.coordination_steps(id).unwrap(),
        [CoordinationStep {
            step: 0,
            batch: None,
            attempt: Some(attempt),
            insert_batch: None,
            metadata_batch: None,
        }]
    );
    // An outstanding attempt blocks closing.
    let error = failure(j.complete_coordination(id, 1, b"done"));
    assert!(error.contains("outstanding attempt"), "{error}");
    worker.cancel_prepared(attempt).unwrap();
    // A coordinated batch is the next step; it blocks closing while outstanding.
    let mut request = intent(vec![unowned(4242, "iTunes_Control/Music/F40/U001.mp3")]);
    request.coordination = Some(id);
    let batch = j.prepare_removal_batch(&request).unwrap();
    assert_eq!(j.removal_batch(batch).unwrap().coordination, Some(id));
    assert_eq!(
        j.coordination_steps(id).unwrap()[1],
        CoordinationStep {
            step: 1,
            batch: Some(batch),
            attempt: None,
            insert_batch: None,
            metadata_batch: None,
        }
    );
    assert!(failure(j.complete_coordination(id, 1, b"done")).contains("outstanding removal batch"));
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(batch, [9; 32], 14).unwrap();
    // A batch may only extend the Active coordination it names.
    let mut stray = intent(vec![unowned(4243, "iTunes_Control/Music/F40/U002.mp3")]);
    stray.coordination = Some(CoordinationId(id.as_i64() + 5));
    assert!(failure(j.prepare_removal_batch(&stray)).contains("not Active"));
    // The permit ends with its guard; the coordination still owns the journal.
    drop(permit);
    assert!(failure(try_prepare_insert(&mut worker, alac_content(3))).contains("owns the journal"));
    j.complete_coordination(id, 1, b"done").unwrap();
    // Closed, the steps stay as history and ordinary writes resume.
    assert_eq!(j.coordination_steps(id).unwrap().len(), 2);
    insert_verified(&mut worker, alac_content(3));
    assert!(failure(j.enter_coordination(id)).contains("not the Active coordination"));
    // Steps are immutable history.
    for sql in [
        "UPDATE coordination_steps SET step=5",
        "DELETE FROM coordination_steps",
        "INSERT INTO coordination_steps(coordination,step,batch,attempt) VALUES(1,2,NULL,1)",
    ] {
        sql_error(&worker, sql);
    }
    drop((j, worker));
    Journal::open(&t.path(), &identity()).unwrap();
}
#[test]
fn coordination_step_order_is_enforced_in_sql() {
    let t = Temp::new();
    let mut j = created(&t);
    let id = j
        .begin_coordination(CoordinationKind::Clear, b"clear", Path::new("/run/x"), 0)
        .unwrap();
    // A step must be the next one, for an Active coordination, naming one thing.
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO coordination_steps(coordination,step,batch,attempt) VALUES({},1,NULL,1)",
            id.as_i64()
        ),
    );
    assert!(error.contains("in order"), "{error}");
    sql_error(
        &j,
        &format!(
            "INSERT INTO coordination_steps(coordination,step,batch,attempt) VALUES({},0,NULL,NULL)",
            id.as_i64()
        ),
    );
    sql_error(
        &j,
        "INSERT INTO coordination_steps(coordination,step,batch,attempt) VALUES(99,0,NULL,1)",
    );
    j.complete_coordination(id, 0, b"done").unwrap();
    let error = sql_error(
        &j,
        &format!(
            "INSERT INTO coordination_steps(coordination,step,batch,attempt) VALUES({},0,NULL,1)",
            id.as_i64()
        ),
    );
    assert!(error.contains("in order"), "{error}");
}

#[test]
fn removed_targets_stay_reserved_and_are_never_reallocated() {
    let t = Temp::new();
    let mut j = created(&t);
    let (track, path) = (4242_u64, "iTunes_Control/Music/F41/ZZZZ.m4a");
    let id = j
        .prepare_removal_batch(&intent(vec![unowned(track, path)]))
        .unwrap();
    // Even before it is sent, and after it is cancelled.
    assert!(j.reserved_pids().unwrap().contains(&track));
    assert!(
        j.reserved_paths()
            .unwrap()
            .contains(&path.to_ascii_lowercase())
    );
    j.cancel_removal_batch(id).unwrap();
    assert!(j.reserved_pids().unwrap().contains(&track));
    assert!(
        j.reserved_paths()
            .unwrap()
            .contains(&path.to_ascii_lowercase())
    );
    // Neither the identity nor the path can be inserted.
    let mut same_track = alac_content(5);
    same_track.track_pid = track;
    let error = failure(try_prepare_insert(&mut j, same_track));
    assert!(error.contains("removed catalogue row"), "{error}");
    let mut same_path = alac_content(5);
    same_path.path = path
        .to_ascii_uppercase()
        .replace("ITUNES_CONTROL/MUSIC", "iTunes_Control/Music")
        .replace(".M4A", ".m4a");
    let error = failure(try_prepare_insert(&mut j, same_path));
    assert!(error.contains("removed catalogue row"), "{error}");
    // An unrelated insert is unaffected, and nothing was left half-written.
    insert_verified(&mut j, alac_content(6));
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    // Reservation encodings match the observed ones: 8-byte ids, folded paths.
    let ids = reservation_values(&j.connection, 1).unwrap();
    assert!(ids.contains(&track.to_be_bytes().to_vec()));
    let paths = reservation_values(&j.connection, 3).unwrap();
    assert!(paths.contains(path.to_ascii_lowercase().as_bytes()));
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

// --- status ----------------------------------------------------------------------------

#[test]
fn status_shows_the_coordination_and_the_outstanding_batch() {
    let t = Temp::new();
    let (mut j, a, _b) = with_bindings(&t);
    let render = |j: &Journal| {
        let mut text = Vec::new();
        crate::cli::journal_view::write(j, false, &mut text, "").unwrap();
        let mut json = Vec::new();
        crate::cli::journal_view::write(j, true, &mut json, "").unwrap();
        (
            String::from_utf8(text).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&json).unwrap(),
        )
    };
    let (text, json) = render(&j);
    assert!(
        text.contains("Removal batches: 0 verified · 0 not applied"),
        "{text}"
    );
    assert!(json["coordination"].is_null());
    assert!(json["removal_batches"]["outstanding"].is_null());
    let id = j
        .begin_coordination(
            CoordinationKind::ReplaceTrack,
            b"intent",
            Path::new("/run/coordination-fixture"),
            14,
        )
        .unwrap();
    j.advance_coordination(id, 0, 2).unwrap();
    let (text, json) = render(&j);
    assert!(
        text.contains("Coordination 1: track replacement · phase 2 · revision 1 · evidence /run/coordination-fixture"),
        "{text}"
    );
    assert!(text.contains("coordination 1 owns the journal"), "{text}");
    assert_eq!(json["coordination"]["phase"], 2);
    assert_eq!(json["coordination"]["kind"], "replace_track");
    let mut request = intent(vec![
        owned(&j, &a),
        unowned(4242, "iTunes_Control/Music/F40/U001.mp3"),
    ]);
    request.coordination = Some(id);
    let batch = {
        let _guard = permits();
        let _permit = j.enter_coordination(id).unwrap();
        j.prepare_removal_batch(&request).unwrap()
    };
    let (text, json) = render(&j);
    assert!(
        text.contains("Removal batch 1: prepared · 2 targets · base anchor 13"),
        "{text}"
    );
    assert!(text.contains("A removal batch is outstanding"), "{text}");
    assert_eq!(json["removal_batches"]["outstanding"]["state"], "prepared");
    assert_eq!(json["removal_batches"]["outstanding"]["target_count"], 2);
    assert_eq!(json["removal_batches"]["prepared_count"], 1);
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(batch, [9; 32], 14).unwrap();
    j.complete_coordination(id, 1, b"done").unwrap();
    let (text, json) = render(&j);
    assert!(
        text.contains("Removal batches: 1 verified · 0 not applied"),
        "{text}"
    );
    assert!(!text.contains("Coordination 1"), "{text}");
    assert_eq!(json["removal_batches"]["verified_count"], 1);
    assert!(json["coordination"].is_null());
    assert_eq!(json["history"]["retired_binding_count"], 1);
}
