use super::*;

pub(super) fn artwork(j: &mut Journal, mut content: Content, cache: u32) -> Content {
    content.artwork = Some(Artwork {
        cache_id: cache,
        bytes: 100,
        sha256: [44; 32],
    });
    let op = operation(AttemptKind::Artwork, &content, 14, None, None);
    let id = j
        .prepare(PreparedAttempt {
            kind: AttemptKind::Artwork,
            content: content.clone(),
            new_album_pid: None,
            new_artist_pid: None,
            current_media_anchor: 14,
            operation: op,
            input_receipt_sha256: [7; 32],
        })
        .unwrap();
    let permit = j.mark_started(id, start(14)).unwrap();
    j.complete(permit, verified(&content, 15)).unwrap();
    content
}

#[test]
fn history_is_append_only_and_current_aliases_are_active_only() {
    let temp = Temp::new();
    let mut j = Journal::create(&temp.journal(), &scope()).unwrap();
    let original = complete_insert(&mut j, 1);
    let id = j
        .active_binding_id(&original.source.audio)
        .unwrap()
        .unwrap();
    let changed = artwork(&mut j, original.clone(), 900);
    let latest = artwork(&mut j, changed.clone(), 901);
    let all = j.binding_history(id, 0, 100).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[0].binding.content == original && !all[0].active);
    assert!(all[1].binding.content == changed && !all[1].active);
    assert!(all[2].binding.content == latest && all[2].active);
    assert_eq!(j.binding_history(id, 1, 1).unwrap()[0].version, 2);
    assert!(j.binding_history(id, 0, 0).is_err());
    assert!(j.binding_history(id, 0, 1001).is_err());
    assert!(
        j.lookup_active_verified(&original.source.audio)
            .unwrap()
            .unwrap()
            .content
            == latest
    );
    assert_eq!(j.active_verified_bindings().unwrap().len(), 1);
    for sql in [
        "UPDATE binding_records SET provenance=2",
        "DELETE FROM binding_records",
        "UPDATE binding_versions SET anchor=99",
        "DELETE FROM binding_versions",
        "DELETE FROM active_bindings",
        "UPDATE bindings SET path='x'",
    ] {
        assert!(j.connection.execute(sql, []).is_err(), "{sql}");
    }
    drop(j);
    let reopened = Journal::open_read_only(&temp.journal(), &scope()).unwrap();
    assert_eq!(reopened.binding_history(id, 0, 10).unwrap().len(), 3);
    assert!(
        reopened
            .lookup_active_verified(&original.source.audio)
            .unwrap()
            .unwrap()
            .content
            == latest
    );
    assert!(
        reopened
            .reserved_cache_ids()
            .unwrap()
            .is_superset(&BTreeSet::from([900, 901]))
    );
}

#[test]
fn started_checks_headroom_but_completion_may_consume_reserved_capacity() {
    for before_permit in [true, false] {
        let temp = Temp::new();
        let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
        let prepared = insert(1);
        let content = prepared.content.clone();
        let id = journal.prepare(prepared).unwrap();
        let permit = if before_permit {
            None
        } else {
            Some(journal.mark_started(id, start(13)).unwrap())
        };
        // Grow the synthetic file's freelist without changing journal rows or
        // canonical schema. This represents reserved space becoming allocated.
        journal.connection.execute_batch("CREATE TABLE capacity_padding(value BLOB); INSERT INTO capacity_padding VALUES(zeroblob(63963136)); DROP TABLE capacity_padding;").unwrap();
        assert!(bindings::database_bound(&journal.connection, 4 * 1024 * 1024).is_err());
        bindings::database_bound(&journal.connection, 0).unwrap();
        if let Some(permit) = permit {
            journal.complete(permit, verified(&content, 14)).unwrap();
            assert!(
                journal
                    .lookup_active_verified(&content.source.audio)
                    .unwrap()
                    .is_some()
            );
            assert!(journal.outstanding().unwrap().is_none());
        } else {
            assert!(journal.mark_started(id, start(13)).is_err());
            assert_eq!(
                journal.outstanding().unwrap().unwrap().state,
                AttemptState::Prepared
            );
            assert!(journal.start_evidence(id).unwrap().is_none());
        }
        validate_schema(&journal.connection).unwrap();
    }
}

#[test]
fn completed_insert_without_binding_history_is_rejected_on_reopen() {
    let temp = Temp::new();
    let j = Journal::create(&temp.journal(), &scope()).unwrap();
    let p = insert(1);
    let sha: [u8; 32] = Sha256::digest(&p.operation).into();
    j.connection.execute("INSERT INTO attempts(kind,state,content,operation,operation_sha,album,artist,base_anchor,input_receipt,ready_receipt,signature,verified_receipt,verified_anchor) VALUES(1,2,?1,?2,?3,?4,?5,13,?6,?7,?8,?9,14)",params![encode_content(&p.content).unwrap(),p.operation,sha,p.new_album_pid.map(u64::to_be_bytes),p.new_artist_pid.map(u64::to_be_bytes),[7u8;32],[8u8;32],[1u8;21],[9u8;32]]).unwrap();
    assert!(
        j.connection
            .query_row("PRAGMA foreign_key_check", [], |r| r.get::<_, String>(0))
            .optional()
            .unwrap()
            .is_none()
    );
    drop(j);
    assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
}

#[test]
fn completed_event_with_invalid_started_evidence_is_rejected() {
    for assignment in [
        "signature=zeroblob(21),ready_receipt=zeroblob(32)",
        "signature=zeroblob(21)",
        "ready_receipt=zeroblob(32)",
        "verified_receipt=zeroblob(32)",
        "verified_anchor=base_anchor",
    ] {
        let temp = Temp::new();
        let mut j = Journal::create(&temp.journal(), &scope()).unwrap();
        complete_insert(&mut j, 1);
        drop(j);
        let j = Journal::open(&temp.journal(), &scope()).unwrap();
        j.connection
            .execute_batch(&format!("UPDATE attempts SET {assignment} WHERE state=2"))
            .unwrap();
        drop(j);
        assert!(
            Journal::open_read_only(&temp.journal(), &scope()).is_err(),
            "{assignment}"
        );
    }
}

#[test]
fn completed_native_artwork_without_resulting_version_is_rejected_on_reopen() {
    let temp = Temp::new();
    let mut j = Journal::create(&temp.journal(), &scope()).unwrap();
    let mut c = complete_insert(&mut j, 1);
    c.artwork = Some(Artwork {
        cache_id: 900,
        bytes: 100,
        sha256: [44; 32],
    });
    let id = j
        .prepare(PreparedAttempt {
            kind: AttemptKind::Artwork,
            content: c.clone(),
            new_album_pid: None,
            new_artist_pid: None,
            current_media_anchor: 14,
            operation: operation(AttemptKind::Artwork, &c, 14, None, None),
            input_receipt_sha256: [7; 32],
        })
        .unwrap();
    drop(j.mark_started(id, start(14)).unwrap());
    j.connection
        .execute(
            "UPDATE attempts SET state=2,verified_receipt=?1,verified_anchor=15 WHERE id=?2",
            params![[9u8; 32], id.0],
        )
        .unwrap();
    drop(j);
    assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
}
