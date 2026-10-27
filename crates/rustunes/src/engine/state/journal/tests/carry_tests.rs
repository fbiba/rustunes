//! Journals written under other hash definitions or another schema are
//! recognised as superseded, read by `read_prior` and carried into a new
//! journal by `record_pull`.
use super::*;
use std::collections::BTreeSet;

fn superseded<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("the journal must be superseded"),
        Err(error) => {
            assert!(is_superseded(&error), "{error:#}");
            format!("{error:#}")
        }
    }
}

/// Rewrite `journal`'s binding rows as a build whose audio composite has
/// another domain would have written them, keeping its schema exact.
fn rehash_as_another_build(journal: &Path) {
    let c = Connection::open(journal.join(DB_NAME)).unwrap();
    let triggers: Vec<String> = c
        .prepare("SELECT sql FROM sqlite_schema WHERE type='trigger' AND (tbl_name LIKE 'binding%' OR tbl_name='active_bindings')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let names: Vec<String> = c
        .prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND (tbl_name LIKE 'binding%' OR tbl_name='active_bindings')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    c.execute_batch("PRAGMA foreign_keys=OFF; BEGIN").unwrap();
    for name in &names {
        c.execute_batch(&format!("DROP TRIGGER {name}")).unwrap();
    }
    let contents: Vec<(i64, Vec<u8>)> = c
        .prepare("SELECT binding_id,content FROM binding_versions")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (id, mut content) in contents {
        // ALAC content: kind, size, file hash, then the composite.
        assert_eq!(content[0], 1);
        let stale: [u8; 32] =
            Sha256::digest([&b"another-domain\0"[..], &content[41..73]].concat()).into();
        content[41..73].copy_from_slice(&stale);
        c.execute(
            "UPDATE binding_versions SET content=?1 WHERE binding_id=?2",
            params![content, id],
        )
        .unwrap();
        c.execute(
            "UPDATE binding_records SET audio=?1 WHERE id=?2",
            params![stale, id],
        )
        .unwrap();
        c.execute(
            "UPDATE active_bindings SET audio=?1 WHERE binding_id=?2",
            params![stale, id],
        )
        .unwrap();
    }
    for sql in &triggers {
        c.execute_batch(sql).unwrap();
    }
    c.execute_batch("COMMIT").unwrap();
}

#[test]
fn content_hashed_under_another_definition_is_superseded_or_recomputed() {
    let content = test_support::alac_content(1);
    let mut bytes = encode_content(&content).unwrap();
    assert!(decode_content(&bytes).unwrap() == content);
    bytes[41] ^= 1;
    superseded(decode_content(&bytes));
    // Leniently, this build's identity comes back from the stored components.
    assert!(decode_content_as(&bytes, false).unwrap() == content);
}

#[test]
fn a_journal_of_another_schema_or_hash_definition_is_superseded() {
    let t = test_support::Temp::new();
    let mut j = Journal::create(&t.path(), &test_support::identity()).unwrap();
    test_support::insert_verified(&mut j, test_support::alac_content(1));
    drop(j);
    rehash_as_another_build(&t.path());
    let message = superseded(Journal::open_read_only(
        &t.path(),
        &test_support::identity(),
    ));
    assert!(message.contains("rustunes pull"), "{message}");

    let t = test_support::Temp::new();
    drop(Journal::create(&t.path(), &test_support::identity()).unwrap());
    Connection::open(t.path().join(DB_NAME))
        .unwrap()
        .execute_batch("CREATE TABLE later_addition(x)")
        .unwrap();
    let message = superseded(Journal::open(&t.path(), &test_support::identity()));
    assert!(message.contains("schema"), "{message}");
}

#[test]
fn a_pull_carries_an_earlier_journals_bindings_sources_and_reservations() {
    let earlier = test_support::Temp::new();
    let mut j = Journal::create(&earlier.path(), &test_support::identity()).unwrap();
    let single = test_support::alac_content(1);
    test_support::insert_verified(&mut j, single.clone());
    test_support::insert_verified(&mut j, test_support::alac_content(2));
    let covered = super::bindings_tests::artwork(&mut j, test_support::alac_content(2), 140);
    let retired = test_support::alac_content(3);
    test_support::insert_verified(&mut j, retired.clone());
    test_support::retire(&mut j, &retired);
    drop(j);
    rehash_as_another_build(&earlier.path());

    let scratch = earlier.0.join("copy");
    let prior = read_prior(&earlier.path(), &scratch).unwrap();
    assert!(prior.unfinished.is_empty() && prior.unreadable.is_empty());
    // Active bindings only, with this build's audio identities.
    assert_eq!(prior.bindings.len(), 2);
    assert!(prior.bindings[0].content == single && prior.bindings[1].content == covered);
    let has =
        |namespace: i64, value: Vec<u8>| prior.reserved.contains(&Reserved { namespace, value });
    for content in [&single, &covered, &retired] {
        assert!(has(1, content.track_pid.to_be_bytes().to_vec()));
        assert!(has(3, content.path.to_ascii_lowercase().into_bytes()));
    }
    assert!(has(2, 140u32.to_be_bytes().to_vec()));

    let later = test_support::Temp::new();
    let mut j = Journal::create(&later.path(), &test_support::identity()).unwrap();
    let pulled: Vec<Pulled> = prior
        .bindings
        .iter()
        .map(|b| Pulled {
            content: b.content.clone(),
            source: Some(format!("/music/{:016x}.m4a", b.content.track_pid).into()),
            conversion: None,
        })
        .collect();
    j.record_pull(&pulled, &prior.reserved, [5; 32], 20)
        .unwrap();
    // A pull goes only into a new journal.
    assert!(
        j.record_pull(&pulled, &BTreeSet::new(), [5; 32], 20)
            .is_err()
    );
    drop(j);
    let j = Journal::open(&later.path(), &test_support::identity()).unwrap();
    let bindings = j.active_verified_bindings().unwrap();
    assert_eq!(bindings.len(), 2);
    assert!(bindings
        .iter()
        .all(|b| b.receipt_kind == BindingReceiptKind::PullVerification && b.media_anchor == 20));
    assert!(
        j.lookup_active_verified(&covered.source.audio)
            .unwrap()
            .unwrap()
            .content
            == covered
    );
    assert_eq!(j.recorded_sources().unwrap().len(), 2);
    // The retired track's identities stay reserved.
    assert!(j.reserved_pids().unwrap().contains(&retired.track_pid));
    assert!(
        j.reserved_paths()
            .unwrap()
            .contains(&retired.path.to_ascii_lowercase())
    );
    // The earlier journal is untouched.
    superseded(Journal::open_read_only(
        &earlier.path(),
        &test_support::identity(),
    ));
}

#[test]
fn a_single_insertions_recorded_source_is_kept_for_a_later_pull() {
    let t = test_support::Temp::new();
    let mut j = Journal::create(&t.path(), &test_support::identity()).unwrap();
    let content = test_support::alac_content(4);
    test_support::insert_verified(&mut j, content.clone());
    j.record_source(content.track_pid, Path::new("/music/sent.m4a"))
        .unwrap();
    // A binding keeps the first source it records; an unbound track has none.
    j.record_source(content.track_pid, Path::new("/music/other.m4a"))
        .unwrap();
    j.record_source(7, Path::new("/music/unbound.m4a")).unwrap();
    assert!(
        j.record_source(content.track_pid, Path::new("relative.m4a"))
            .is_err()
    );
    let sources = j.recorded_sources().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[&content.track_pid], Path::new("/music/sent.m4a"));
    drop(j);
    let prior = read_prior(&t.path(), &t.0.join("copy")).unwrap();
    assert_eq!(
        prior.bindings[0].source.as_deref(),
        Some(Path::new("/music/sent.m4a"))
    );
}

#[test]
fn run_references_name_each_batchs_evidence_and_each_attempts_input() {
    let t = test_support::Temp::new();
    let mut j = Journal::create(&t.path(), &test_support::identity()).unwrap();
    // Finished work still names its run; planning reads its input again.
    test_support::insert_verified(&mut j, test_support::alac_content(4));
    let cancelled = Path::new("/state/runs/1-2/audio/batch-001");
    let id =
        test_support::prepare_insert_batch_at(&mut j, test_support::alac_content(5), cancelled);
    j.cancel_insert_batch(id).unwrap();
    let prepared = Path::new("/state/runs/3-4/audio/batch-001");
    test_support::prepare_insert_batch_at(&mut j, test_support::alac_content(6), prepared);
    drop(j);
    let references = run_references(&t.path(), &t.0.join("copy")).unwrap();
    assert_eq!(references.unfinished.evidence, vec![prepared.to_path_buf()]);
    assert!(references.unfinished.inputs.is_empty());
    assert_eq!(references.finished.evidence, vec![cancelled.to_path_buf()]);
    assert_eq!(references.finished.inputs, BTreeSet::from([[7; 32]]));
}
