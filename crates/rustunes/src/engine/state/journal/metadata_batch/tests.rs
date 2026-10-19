//! Metadata batches on real temporary journals: lifecycle, refusals, the
//! mutual exclusion of native writes in Rust and SQL, coordination linking
//! and validation of tampered rows.
use super::*;
use crate::engine::state::journal::test_support::{
    Temp, alac_content, identity, insert_started, insert_verified, prepare_insert,
    try_prepare_insert,
};
use coordination::{CoordinationKind, CoordinationStep, RemovalIntent, RemovalTarget};

fn created(t: &Temp) -> Journal {
    Journal::create(&t.path(), &identity()).unwrap()
}
fn rows(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
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
fn tracks() -> Vec<u64> {
    vec![0x1111, 0x2222, 0x3333]
}
fn intent(tracks: &[u64]) -> MetadataIntent {
    MetadataIntent {
        coordination: None,
        operation: vec![6; 40],
        base_anchor: 13,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/metadata-fixture".into(),
        tracks: tracks.to_vec(),
    }
}
fn prepared(j: &mut Journal) -> MetadataBatchId {
    j.prepare_metadata_batch(&intent(&tracks())).unwrap()
}
fn started(j: &mut Journal) -> MetadataBatchId {
    let id = prepared(j);
    j.start_metadata_batch(id, [8; 32], [1; 21]).unwrap();
    id
}
fn removal(targets: Vec<RemovalTarget>) -> RemovalIntent {
    RemovalIntent {
        coordination: None,
        compensates: None,
        compensates_insert_batch: None,
        operation: vec![5; 40],
        base_anchor: 14,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/removal-fixture".into(),
        targets,
        playlists: Vec::new(),
    }
}
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
fn insert_intent(seed: u8) -> InsertIntent {
    InsertIntent {
        coordination: None,
        operation: vec![5; 40],
        base_anchor: 13,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/insert-fixture".into(),
        targets: vec![InsertTarget {
            content: alac_content(seed),
            new_album_pid: None,
            new_artist_pid: None,
            conversion: None,
            source: None,
        }],
    }
}
const RAW_INSERT_BATCH: &str = "INSERT INTO insert_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_METADATA_BATCH: &str = "INSERT INTO metadata_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_REMOVAL_BATCH: &str = "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_ATTEMPT: &str = "INSERT INTO attempts(kind,state,content,operation,operation_sha,base_anchor,input_receipt) VALUES(1,0,x'01',x'01',zeroblob(32),0,zeroblob(32))";

#[test]
fn metadata_batch_settles_verified_and_changes_no_binding_or_reservation() {
    let t = Temp::new();
    let mut j = created(&t);
    insert_verified(&mut j, alac_content(1));
    let counts = |j: &Journal| {
        [
            "binding_records",
            "binding_versions",
            "active_bindings",
            "reservations",
            "attempts",
        ]
        .map(|table| rows(&j.connection, table))
    };
    let before = counts(&j);
    let id = prepared(&mut j);
    let batch = j.metadata_batch(id).unwrap();
    assert_eq!(batch.state, MetadataBatchState::Prepared);
    assert_eq!(batch.tracks, tracks());
    assert_eq!(batch.coordination, None);
    assert_eq!(batch.operation, vec![6; 40]);
    assert_eq!(batch.evidence, Path::new("/run/metadata-fixture"));
    assert!(batch.ready_receipt.is_none() && batch.signature.is_none());
    assert_eq!(j.outstanding_metadata_batch().unwrap().unwrap(), batch);
    assert_eq!(j.metadata_batches().unwrap(), [batch.clone()]);
    // Prepared owns the journal.
    assert!(
        failure(try_prepare_insert(&mut j, alac_content(9)))
            .contains("outstanding metadata batch requires settlement")
    );
    assert!(
        failure(j.begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 13))
            .contains("outstanding metadata batch")
    );
    // Start is durable and hands out the one permit.
    let permit = j.start_metadata_batch(id, [8; 32], [1; 21]).unwrap();
    assert_eq!(permit.operation(), &[6; 40]);
    assert_eq!(permit.signature(), &[1; 21]);
    let batch = j.metadata_batch(id).unwrap();
    assert_eq!(batch.state, MetadataBatchState::Started);
    assert_eq!(batch.ready_receipt, Some([8; 32]));
    assert_eq!(batch.signature, Some([1; 21]));
    assert!(
        failure(j.start_metadata_batch(id, [8; 32], [1; 21])).contains("already started or closed")
    );
    assert!(failure(j.cancel_metadata_batch(id)).contains("only a Prepared"));
    // A reopened journal holds the Started batch and never re-issues a permit.
    drop(j);
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    assert_eq!(
        j.outstanding_metadata_batch().unwrap().unwrap().state,
        MetadataBatchState::Started
    );
    assert!(
        failure(j.start_metadata_batch(id, [8; 32], [1; 21])).contains("already started or closed")
    );
    // Verification needs a fresh anchor past the batch's revision base.
    assert!(failure(j.settle_metadata_batch(id, [7; 32], 13)).contains("does not cover"));
    assert!(failure(j.settle_metadata_batch(id, [7; 32], 0)).contains("does not cover"));
    assert!(failure(j.settle_metadata_batch(id, [0; 32], 14)).contains("digest"));
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    let batch = j.metadata_batch(id).unwrap();
    assert_eq!(batch.state, MetadataBatchState::Verified);
    assert_eq!(batch.result_receipt, Some([7; 32]));
    assert_eq!(batch.verified_anchor, Some(14));
    assert_eq!(batch.tracks, tracks());
    assert!(j.outstanding_metadata_batch().unwrap().is_none());
    assert!(j.outstanding().unwrap().is_none());
    // No binding or reservation moved, and ordinary writes resume.
    assert_eq!(counts(&j), before);
    assert_eq!(j.active_bindings().unwrap().len(), 1);
    assert!(failure(j.settle_metadata_batch(id, [7; 32], 15)).contains("not Started"));
    insert_verified(&mut j, alac_content(2));
    // A second batch may follow, with the same tracks again.
    let again = prepared(&mut j);
    assert_ne!(again, id);
    j.cancel_metadata_batch(again).unwrap();
    assert_eq!(j.metadata_batches().unwrap().len(), 2);
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
    Journal::open_read_only(&t.path(), &identity()).unwrap();
}

#[test]
fn metadata_batch_cancel_not_applied_and_invalid_transitions() {
    let t = Temp::new();
    let mut j = created(&t);
    // Cancelled: only from Prepared.
    let cancelled = prepared(&mut j);
    assert!(
        failure(j.mark_metadata_batch_not_applied(cancelled, [4; 32])).contains("only a Started")
    );
    assert!(failure(j.settle_metadata_batch(cancelled, [4; 32], 14)).contains("not Started"));
    j.cancel_metadata_batch(cancelled).unwrap();
    assert_eq!(
        j.metadata_batch(cancelled).unwrap().state,
        MetadataBatchState::Cancelled
    );
    assert!(failure(j.cancel_metadata_batch(cancelled)).contains("only a Prepared"));
    assert!(
        failure(j.start_metadata_batch(cancelled, [8; 32], [1; 21]))
            .contains("already started or closed")
    );
    assert!(
        failure(j.mark_metadata_batch_not_applied(cancelled, [4; 32])).contains("only a Started")
    );
    assert!(j.outstanding_metadata_batch().unwrap().is_none());
    // NotApplied: only from Started.
    let sent = started(&mut j);
    assert!(failure(j.mark_metadata_batch_not_applied(sent, [0; 32])).contains("digest"));
    j.mark_metadata_batch_not_applied(sent, [4; 32]).unwrap();
    let record = j.metadata_batch(sent).unwrap();
    assert_eq!(record.state, MetadataBatchState::NotApplied);
    assert_eq!(record.result_receipt, Some([4; 32]));
    assert_eq!(record.verified_anchor, None);
    for error in [
        failure(j.settle_metadata_batch(sent, [4; 32], 14)),
        failure(j.mark_metadata_batch_not_applied(sent, [4; 32])),
        failure(j.cancel_metadata_batch(sent)),
        failure(j.start_metadata_batch(sent, [8; 32], [1; 21])),
    ] {
        assert!(
            error.contains("not Started")
                || error.contains("only a")
                || error.contains("already started or closed"),
            "{error}"
        );
    }
    assert!(j.outstanding_metadata_batch().unwrap().is_none());
    // Unknown identifiers.
    let missing = MetadataBatchId(sent.as_i64() + 50);
    assert!(failure(j.metadata_batch(missing)).contains("not found"));
    assert!(failure(j.settle_metadata_batch(missing, [4; 32], 14)).contains("not found"));
    assert!(failure(j.start_metadata_batch(missing, [8; 32], [1; 21])).contains("not found"));
    assert!(failure(j.cancel_metadata_batch(missing)).contains("only a Prepared"));
    assert!(failure(j.start_metadata_batch(sent, [0; 32], [1; 21])).contains("digest"));
    assert!(failure(j.start_metadata_batch(sent, [8; 32], [2; 21])).contains("signature"));
    // The SQL enforces the same graph and keeps history.
    let done = started(&mut j);
    j.settle_metadata_batch(done, [7; 32], 14).unwrap();
    let live = prepared(&mut j);
    let (cancelled_id, sent_id, done_id, live_id) = (
        cancelled.as_i64(),
        sent.as_i64(),
        done.as_i64(),
        live.as_i64(),
    );
    for (sql, message) in [
        (
            format!("UPDATE metadata_batches SET state=2 WHERE id={cancelled_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET state=1 WHERE id={sent_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET state=4 WHERE id={done_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET base_anchor=0 WHERE id={done_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET operation=x'02' WHERE id={live_id}"),
            "preparation is immutable",
        ),
        (
            format!("UPDATE metadata_batches SET coordination=1 WHERE id={live_id}"),
            "preparation is immutable",
        ),
        (
            format!("UPDATE metadata_batches SET evidence='/y' WHERE id={live_id}"),
            "preparation is immutable",
        ),
        (
            format!("DELETE FROM metadata_batches WHERE id={done_id}"),
            "history is retained",
        ),
        (
            format!("UPDATE metadata_targets SET track=zeroblob(8) WHERE batch={done_id}"),
            "immutable",
        ),
        (
            format!("DELETE FROM metadata_targets WHERE batch={done_id}"),
            "history is retained",
        ),
        (
            format!("INSERT INTO metadata_targets VALUES({done_id},3,zeroblob(8))"),
            "extends a Prepared batch",
        ),
        (
            format!("INSERT INTO metadata_targets VALUES({live_id},5,zeroblob(8))"),
            "extends a Prepared batch",
        ),
        (
            format!("INSERT INTO metadata_targets VALUES({live_id},3,x'0000000000001111')"),
            "UNIQUE",
        ),
        // From Prepared only 1 and 3, from Started only 2 and 4.
        (
            format!("UPDATE metadata_batches SET state=2 WHERE id={live_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET state=4 WHERE id={live_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET state=5 WHERE id={live_id}"),
            "metadata batch",
        ),
        (
            format!("UPDATE metadata_batches SET state=1 WHERE id={live_id}"),
            "CHECK",
        ),
        (
            format!(
                "UPDATE metadata_batches SET state=1,ready_receipt=zeroblob(32),signature=zeroblob(21) WHERE id={live_id}; UPDATE metadata_batches SET state=3,result_receipt=zeroblob(32) WHERE id={live_id}"
            ),
            "metadata batch",
        ),
        (
            format!(
                "UPDATE metadata_batches SET state=1,ready_receipt=zeroblob(32),signature=zeroblob(21) WHERE id={live_id}; UPDATE metadata_batches SET ready_receipt=zeroblob(32) WHERE id={live_id}"
            ),
            "start is immutable",
        ),
        (
            format!(
                "UPDATE metadata_batches SET state=1,ready_receipt=zeroblob(32),signature=zeroblob(21) WHERE id={live_id}; UPDATE metadata_batches SET state=2,result_receipt=zeroblob(32) WHERE id={live_id}"
            ),
            "CHECK",
        ),
    ] {
        let error = sql_error(&j, &sql);
        assert!(error.contains(message), "{sql}: {error}");
    }
    j.cancel_metadata_batch(live).unwrap();
    assert_eq!(j.metadata_batches().unwrap().len(), 4);
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn metadata_batch_refuses_malformed_intents_and_the_wrong_journal() {
    let t = Temp::new();
    let mut j = created(&t);
    let bad = |j: &mut Journal, edit: &dyn Fn(&mut MetadataIntent)| -> String {
        let mut request = intent(&tracks());
        edit(&mut request);
        failure(j.prepare_metadata_batch(&request))
    };
    assert!(bad(&mut j, &|i| i.tracks.clear()).contains("between one and 1000"));
    assert!(bad(&mut j, &|i| i.tracks = (1..=1001).collect()).contains("between one and 1000"));
    assert!(bad(&mut j, &|i| i.tracks = vec![1, 2, 1]).contains("twice"));
    assert!(bad(&mut j, &|i| i.tracks = vec![1, 0]).contains("track identity"));
    assert!(bad(&mut j, &|i| i.operation.clear()).contains("operation exceeds bounds"));
    assert!(
        bad(&mut j, &|i| i.operation = vec![1; MAX_OPERATION + 1])
            .contains("operation exceeds bounds")
    );
    assert!(bad(&mut j, &|i| i.base_anchor = u32::MAX).contains("anchor exhausted"));
    assert!(bad(&mut j, &|i| i.before_current = [0; 32]).contains("digest"));
    assert!(bad(&mut j, &|i| i.before_index = [0; 32]).contains("digest"));
    assert!(bad(&mut j, &|i| i.input_receipt = [0; 32]).contains("digest"));
    assert!(bad(&mut j, &|i| i.evidence = "relative".into()).contains("absolute path"));
    assert!(bad(&mut j, &|i| i.evidence = "/".into()).contains("absolute path"));
    assert_eq!(rows(&j.connection, "metadata_batches"), 0);
    assert_eq!(rows(&j.connection, "metadata_targets"), 0);
    // The largest accepted batch.
    let id = j
        .prepare_metadata_batch(&intent(&(1..=1000).collect::<Vec<u64>>()))
        .unwrap();
    assert_eq!(j.metadata_batch(id).unwrap().tracks.len(), 1000);
    j.cancel_metadata_batch(id).unwrap();
    drop(j);
    // A read-only handle cannot write.
    let mut read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert!(failure(read.prepare_metadata_batch(&intent(&tracks()))).contains("read-only"));
    assert!(failure(read.cancel_metadata_batch(MetadataBatchId(1))).contains("read-only"));
}

#[test]
fn native_writes_exclude_metadata_batches_in_rust_and_sql() {
    // An attempt blocks a metadata batch, Started or only Prepared.
    for started_attempt in [true, false] {
        let t = Temp::new();
        let mut j = created(&t);
        if started_attempt {
            insert_started(&mut j, alac_content(1));
        } else {
            prepare_insert(&mut j, alac_content(1));
        }
        assert!(
            failure(j.prepare_metadata_batch(&intent(&tracks()))).contains("outstanding attempt")
        );
        assert!(
            sql_error(&j, RAW_METADATA_BATCH)
                .contains("a metadata batch is Prepared on an idle journal")
        );
    }
    // A removal batch blocks one, Prepared or Started.
    let t = Temp::new();
    let mut j = created(&t);
    let content = alac_content(2);
    insert_verified(&mut j, content.clone());
    let removal_batch = j
        .prepare_removal_batch(&removal(vec![owned(&j, &content)]))
        .unwrap();
    assert!(
        failure(j.prepare_metadata_batch(&intent(&tracks()))).contains("outstanding removal batch")
    );
    assert!(
        sql_error(&j, RAW_METADATA_BATCH)
            .contains("a metadata batch is Prepared on an idle journal")
    );
    j.start_removal_batch(removal_batch, [8; 32], [1; 21])
        .unwrap();
    assert!(
        failure(j.prepare_metadata_batch(&intent(&tracks()))).contains("outstanding removal batch")
    );
    j.settle_removal_batch(removal_batch, [6; 32], 15).unwrap();
    // An insert batch blocks one in every outstanding state.
    let insert = j.prepare_insert_batch(&insert_intent(4)).unwrap();
    assert!(
        failure(j.prepare_metadata_batch(&intent(&tracks()))).contains("outstanding insert batch")
    );
    assert!(
        sql_error(&j, RAW_METADATA_BATCH)
            .contains("a metadata batch is Prepared on an idle journal")
    );
    j.start_insert_batch(insert, [8; 32], [1; 21]).unwrap();
    assert!(
        failure(j.prepare_metadata_batch(&intent(&tracks()))).contains("outstanding insert batch")
    );
    j.mark_insert_batch_not_applied(insert, [4; 32]).unwrap();
    // Idle again; now a metadata batch blocks everything else, Prepared or Started.
    for start in [false, true] {
        let id = prepared(&mut j);
        if start {
            j.start_metadata_batch(id, [8; 32], [1; 21]).unwrap();
        }
        let message = "outstanding metadata batch requires settlement";
        assert!(failure(try_prepare_insert(&mut j, alac_content(9))).contains(message));
        assert!(failure(j.prepare_insert_batch(&insert_intent(9))).contains(message));
        assert!(failure(j.prepare_metadata_batch(&intent(&tracks()))).contains(message));
        assert!(
            failure(j.prepare_removal_batch(&removal(vec![locationless(0x4444)])))
                .contains(message)
        );
        let mut compensating = removal(vec![locationless(0x4444)]);
        compensating.compensates_insert_batch = Some(insert);
        assert!(failure(j.prepare_removal_batch(&compensating)).contains(message));
        assert!(
            failure(j.begin_coordination(
                CoordinationKind::Clear,
                b"intent",
                Path::new("/run/x"),
                14
            ))
            .contains("outstanding metadata batch")
        );
        assert!(sql_error(&j, RAW_ATTEMPT).contains(message));
        assert!(
            sql_error(&j, RAW_REMOVAL_BATCH)
                .contains("a removal batch is Prepared on an idle journal")
        );
        assert!(
            sql_error(&j, RAW_INSERT_BATCH)
                .contains("an insert batch is Prepared on an idle journal")
        );
        assert!(
            sql_error(&j, RAW_METADATA_BATCH)
                .contains("a metadata batch is Prepared on an idle journal")
        );
        if start {
            j.mark_metadata_batch_not_applied(id, [4; 32]).unwrap();
        } else {
            j.cancel_metadata_batch(id).unwrap();
        }
        assert!(j.outstanding_metadata_batch().unwrap().is_none());
    }
    // Settled, everything is admitted again.
    insert_verified(&mut j, alac_content(9));
    let id = started(&mut j);
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    let id = j.prepare_insert_batch(&insert_intent(10)).unwrap();
    j.cancel_insert_batch(id).unwrap();
}

#[test]
fn coordinated_metadata_batch_links_its_step_and_needs_an_active_coordination() {
    let t = Temp::new();
    let mut j = created(&t);
    let c = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 13)
        .unwrap();
    // The coordination owns the journal: an unowned batch is refused without its permit.
    let error = failure(j.prepare_metadata_batch(&intent(&tracks())));
    assert!(error.contains("owns the journal"), "{error}");
    // A batch may only extend the Active coordination it names.
    let mut stray = intent(&tracks());
    stray.coordination = Some(CoordinationId(c.as_i64() + 5));
    assert!(failure(j.prepare_metadata_batch(&stray)).contains("not Active"));
    assert_eq!(rows(&j.connection, "metadata_batches"), 0);
    let mut request = intent(&tracks());
    request.coordination = Some(c);
    let id = j.prepare_metadata_batch(&request).unwrap();
    assert_eq!(j.metadata_batch(id).unwrap().coordination, Some(c));
    assert_eq!(
        j.coordination_steps(c).unwrap(),
        [CoordinationStep {
            step: 0,
            batch: None,
            attempt: None,
            insert_batch: None,
            metadata_batch: Some(id),
        }]
    );
    // It blocks closing the coordination while outstanding.
    assert!(failure(j.complete_coordination(c, 0, b"done")).contains("outstanding metadata batch"));
    j.start_metadata_batch(id, [8; 32], [1; 21]).unwrap();
    assert!(failure(j.complete_coordination(c, 0, b"done")).contains("outstanding metadata batch"));
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    // A second coordinated batch is the next step; so is an insert batch after it.
    let mut next = intent(&[0x5555]);
    next.coordination = Some(c);
    let second = j.prepare_metadata_batch(&next).unwrap();
    j.cancel_metadata_batch(second).unwrap();
    let mut insert = insert_intent(4);
    insert.coordination = Some(c);
    let third = j.prepare_insert_batch(&insert).unwrap();
    j.cancel_insert_batch(third).unwrap();
    let steps = j.coordination_steps(c).unwrap();
    assert_eq!(
        steps.iter().map(|s| s.metadata_batch).collect::<Vec<_>>(),
        [Some(id), Some(second), None]
    );
    assert_eq!(steps[2].insert_batch, Some(third));
    j.complete_coordination(c, 0, b"done").unwrap();
    // Closed, the steps stay as history; a closed coordination takes no more.
    assert_eq!(j.coordination_steps(c).unwrap().len(), 3);
    assert!(failure(j.prepare_metadata_batch(&request)).contains("not Active"));
    drop(j);
    // The SQL of the step table: exactly one link, each once, in order.
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    let c2 = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/y"), 14)
        .unwrap();
    let c2 = c2.as_i64();
    for (sql, message) in [
        (
            format!("INSERT INTO coordination_steps(coordination,step) VALUES({c2},0)"),
            "CHECK",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,batch,metadata_batch) VALUES({c2},0,1,{})",
                id.as_i64()
            ),
            "CHECK",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,metadata_batch) VALUES({c2},1,{})",
                id.as_i64()
            ),
            "extends an Active coordination in order",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,metadata_batch) VALUES({c2},0,{})",
                id.as_i64()
            ),
            "UNIQUE",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,metadata_batch) VALUES({c2},0,999)"
            ),
            "FOREIGN KEY",
        ),
        (
            format!(
                "UPDATE coordination_steps SET metadata_batch=NULL WHERE metadata_batch={}",
                id.as_i64()
            ),
            "immutable",
        ),
        ("DELETE FROM coordination_steps".to_string(), "retained"),
        // A coordinated batch names an Active coordination.
        (
            format!(
                "INSERT INTO metadata_batches(coordination,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
                c.as_i64()
            ),
            "names an Active coordination",
        ),
    ] {
        let error = sql_error(&j, &sql);
        assert!(error.contains(message), "{sql}: {error}");
    }
    j.complete_coordination(CoordinationId(c2), 0, b"done")
        .unwrap();
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn validation_refuses_tampered_metadata_rows() {
    // Rows a trigger would refuse are written with the trigger set aside and
    // restored from its own text, so only the cell-level validation can object.
    let tamper = |j: &Journal, triggers: &[&str], sql: &str| {
        let saved: Vec<String> = triggers
            .iter()
            .map(|name| {
                j.connection
                    .query_row(
                        "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name=?1",
                        [name],
                        |r| r.get(0),
                    )
                    .unwrap()
            })
            .collect();
        for name in triggers {
            j.connection
                .execute_batch(&format!("DROP TRIGGER {name}"))
                .unwrap();
        }
        j.connection.execute_batch(sql).unwrap();
        for sql in saved {
            j.connection.execute_batch(&sql).unwrap();
        }
    };
    // A settled batch that lost its targets.
    let t = Temp::new();
    let mut j = created(&t);
    let id = started(&mut j);
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    validate_schema(&j.connection).unwrap();
    tamper(
        &j,
        &["metadata_target_retained"],
        &format!("DELETE FROM metadata_targets WHERE batch={}", id.as_i64()),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("between one and 1000"), "{error}");
    // A target ordinal gap.
    let t = Temp::new();
    let mut j = created(&t);
    let id = prepared(&mut j);
    tamper(
        &j,
        &["metadata_target_retained"],
        &format!(
            "DELETE FROM metadata_targets WHERE batch={} AND ordinal=1",
            id.as_i64()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("gap"), "{error}");
    // An operation that no longer matches its digest.
    let t = Temp::new();
    let mut j = created(&t);
    let id = prepared(&mut j);
    tamper(
        &j,
        &["metadata_batch_immutable"],
        &format!(
            "UPDATE metadata_batches SET operation=x'0102' WHERE id={}",
            id.as_i64()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("digest"), "{error}");
    // A coordination column the step table does not back.
    let t = Temp::new();
    let mut j = created(&t);
    let id = started(&mut j);
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    let c = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 14)
        .unwrap();
    tamper(
        &j,
        &["metadata_batch_terminal", "metadata_batch_immutable"],
        &format!(
            "UPDATE metadata_batches SET coordination={} WHERE id={}",
            c.as_i64(),
            id.as_i64()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(
        error.contains("coordination differs from its step"),
        "{error}"
    );
    // A verified anchor that does not cover the revision.
    let t = Temp::new();
    let mut j = created(&t);
    let id = started(&mut j);
    j.settle_metadata_batch(id, [7; 32], 14).unwrap();
    tamper(
        &j,
        &["metadata_batch_terminal"],
        &format!(
            "UPDATE metadata_batches SET verified_anchor=13 WHERE id={}",
            id.as_i64()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("anchor"), "{error}");
    // Two native writes outstanding together, in each pairing.
    for other in ["attempt", "removal", "insert"] {
        let t = Temp::new();
        let mut j = created(&t);
        match other {
            "attempt" => {
                prepare_insert(&mut j, alac_content(1));
            }
            "removal" => {
                let content = alac_content(1);
                insert_verified(&mut j, content.clone());
                j.prepare_removal_batch(&removal(vec![owned(&j, &content)]))
                    .unwrap();
            }
            _ => {
                j.prepare_insert_batch(&insert_intent(4)).unwrap();
            }
        }
        tamper(&j, &["metadata_batch_prepared"], RAW_METADATA_BATCH);
        let error = failure(insertion_batch::validate_exclusion(&j.connection));
        assert!(error.contains("outstanding together"), "{other}: {error}");
    }
}
