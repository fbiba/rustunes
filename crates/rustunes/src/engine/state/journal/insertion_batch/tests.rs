//! Insert batches on real temporary journals (lifecycle, Partial and its
//! compensation, refusals), the mutual exclusion of native writes in Rust and
//! SQL, coordination linking, and the binding and step tables.
use super::*;
use crate::engine::state::journal::test_support::{
    Temp, alac_content, identity, insert_started, insert_verified, prepare_insert,
    try_prepare_insert,
};
use coordination::{BatchState, CoordinationKind, CoordinationStep, RemovalIntent, RemovalTarget};

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
fn idle_clean(j: &Journal) {
    assert!(j.outstanding_insert_batch().unwrap().is_none());
    assert!(j.outstanding_removal_batch().unwrap().is_none());
    assert!(j.outstanding_metadata_batch().unwrap().is_none());
    assert!(j.outstanding().unwrap().is_none());
}

fn target(seed: u8) -> InsertTarget {
    InsertTarget {
        content: alac_content(seed),
        new_album_pid: None,
        new_artist_pid: None,
        conversion: None,
        source: None,
    }
}
fn album(seed: u8) -> u64 {
    0xA000 + u64::from(seed)
}
fn artist(seed: u8) -> u64 {
    0xB000 + u64::from(seed)
}
/// A batch of `seeds`; the first two share a new album, the first also a new
/// artist. Batches with different first seeds allocate different collections.
fn intent(seeds: &[u8]) -> InsertIntent {
    let mut targets: Vec<InsertTarget> = seeds.iter().map(|s| target(*s)).collect();
    if let Some(first) = targets.first_mut() {
        first.new_album_pid = Some(album(seeds[0]));
        first.new_artist_pid = Some(artist(seeds[0]));
    }
    if let Some(second) = targets.get_mut(1) {
        second.new_album_pid = Some(album(seeds[0]));
    }
    InsertIntent {
        coordination: None,
        operation: vec![5; 40],
        base_anchor: 13,
        before_current: [1; 32],
        before_index: [2; 32],
        input_receipt: [3; 32],
        evidence: "/run/insert-fixture".into(),
        targets,
    }
}
fn claim(content: &Content, anchor: u32) -> CompletionEvidence {
    CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: content.artwork.as_ref().map(|a| a.sha256),
        media_anchor: anchor,
        receipt_sha256: [9; 32],
    }
}
fn claims(j: &Journal, id: InsertBatchId, ordinals: &[usize]) -> Vec<(usize, CompletionEvidence)> {
    claims_at(j, id, ordinals, 14)
}
fn claims_at(
    j: &Journal,
    id: InsertBatchId,
    ordinals: &[usize],
    anchor: u32,
) -> Vec<(usize, CompletionEvidence)> {
    let batch = j.insert_batch(id).unwrap();
    ordinals
        .iter()
        .map(|n| (*n, claim(&batch.targets[*n].target.content, anchor)))
        .collect()
}
fn cover(cache_id: u32) -> Artwork {
    Artwork {
        cache_id,
        bytes: 64,
        sha256: [cache_id as u8; 32],
    }
}
fn metadata_intent(tracks: &[u64]) -> MetadataIntent {
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
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// The filename of a music path in lower case, the rest as it was.
fn lower_name(path: &str) -> String {
    let (directory, name) = path.rsplit_once('/').unwrap();
    format!("{directory}/{}", name.to_lowercase())
}
fn prepared(j: &mut Journal, seeds: &[u8]) -> InsertBatchId {
    j.prepare_insert_batch(&intent(seeds)).unwrap()
}
fn started(j: &mut Journal, seeds: &[u8]) -> InsertBatchId {
    let id = prepared(j, seeds);
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    id
}
fn verified(j: &mut Journal, seeds: &[u8]) -> InsertBatchId {
    let id = started(j, seeds);
    let all: Vec<usize> = (0..seeds.len()).collect();
    let bound = claims(j, id, &all);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap(),
        InsertBatchState::Verified
    );
    id
}
/// Three targets, two bound and the middle one a catalogue row without a file.
fn partial(j: &mut Journal) -> InsertBatchId {
    let id = started(j, &[4, 5, 6]);
    let bound = claims(j, id, &[0, 2]);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap(),
        InsertBatchState::Partial
    );
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
fn compensation(batch: InsertBatchId, tracks: &[u64]) -> RemovalIntent {
    let mut request = removal(tracks.iter().map(|t| locationless(*t)).collect());
    request.compensates_insert_batch = Some(batch);
    request
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
fn track(seed: u8) -> u64 {
    alac_content(seed).track_pid
}

const RAW_INSERT_BATCH: &str = "INSERT INTO insert_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_METADATA_BATCH: &str = "INSERT INTO metadata_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_REMOVAL_BATCH: &str = "INSERT INTO removal_batches(state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')";
const RAW_ATTEMPT: &str = "INSERT INTO attempts(kind,state,content,operation,operation_sha,base_anchor,input_receipt) VALUES(1,0,x'01',x'01',zeroblob(32),0,zeroblob(32))";

// --- reference DDL and creation -------------------------------------------

#[test]
fn created_journal_has_empty_batch_storage_and_reopens() {
    let t = Temp::new();
    let j = created(&t);
    let names: BTreeSet<String> = definitions(&j.connection)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for name in [
        "insert_batches",
        "insert_targets",
        "metadata_batches",
        "metadata_targets",
        "one_outstanding_insert_batch",
        "one_outstanding_metadata_batch",
        "one_insert_compensation_batch",
        "attempt_insert_batch_outstanding",
        "attempt_metadata_batch_outstanding",
        "binding_tombstones",
        "coordinations",
        "removal_batches",
    ] {
        assert!(names.contains(name), "{name}");
    }
    assert!(j.route_owner().unwrap().is_none());
    assert!(j.insert_batches().unwrap().is_empty());
    assert!(j.metadata_batches().unwrap().is_empty());
    assert_eq!(
        j.insert_batch_counts().unwrap(),
        InsertBatchCounts::default()
    );
    drop(j);
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    idle_clean(&read);
    drop(read);
    let mut writer = Journal::open(&t.path(), &identity()).unwrap();
    insert_verified(&mut writer, alac_content(1));
    assert_eq!(writer.active_bindings().unwrap().len(), 1);
    drop(writer);
    Journal::open(&t.path(), &identity()).unwrap();
}

// --- conversion --------------------------------------------------------------

#[test]
fn read_only_journal_cannot_write_insert_batches() {
    let t = Temp::new();
    drop(created(&t));
    let mut read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert!(failure(read.prepare_insert_batch(&intent(&[4]))).contains("read-only"));
}

// --- insert batches --------------------------------------------------------------

#[test]
fn insert_batch_settles_verified_binds_every_target_and_a_removal_batch_retires_one() {
    let t = Temp::new();
    let mut j = created(&t);
    let id = prepared(&mut j, &[4, 5, 6]);
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.state, InsertBatchState::Prepared);
    assert_eq!(batch.targets.len(), 3);
    assert!(
        batch
            .targets
            .iter()
            .all(|t| t.outcome.is_none() && t.binding.is_none())
    );
    assert_eq!(batch.targets[0].target.new_album_pid, Some(album(4)));
    assert_eq!(batch.targets[1].target.new_album_pid, Some(album(4)));
    assert_eq!(batch.targets[2].target.new_album_pid, None);
    assert_eq!(j.outstanding_insert_batch().unwrap().unwrap().id, id);
    assert_eq!(j.insert_batch_counts().unwrap().prepared, 1);
    // Tracks, paths and the new collections are reserved by the batch.
    let reserved = |j: &Journal| -> Vec<(i64, Vec<u8>, Option<i64>, bool, Option<i64>)> {
        j.connection
            .prepare("SELECT namespace,value,attempt,consumed,insert_batch FROM reservations ORDER BY namespace,value")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let before = reserved(&j);
    assert_eq!(before.len(), 8);
    assert_eq!(before.iter().filter(|r| r.0 == 1).count(), 5);
    assert_eq!(before.iter().filter(|r| r.0 == 3).count(), 3);
    assert!(
        before
            .iter()
            .all(|r| r.2.is_none() && !r.3 && r.4 == Some(id.as_i64()))
    );
    let pids = j.reserved_pids().unwrap();
    assert!(pids.contains(&album(4)) && pids.contains(&artist(4)) && pids.contains(&track(5)));
    // The batch owns the journal: nothing else may write.
    assert!(
        failure(try_prepare_insert(&mut j, alac_content(9)))
            .contains("outstanding insert batch requires settlement")
    );
    assert!(
        failure(j.prepare_insert_batch(&intent(&[9])))
            .contains("outstanding insert batch requires settlement")
    );
    assert!(
        failure(j.begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 13))
            .contains("outstanding insert batch")
    );
    assert!(
        failure(j.prepare_removal_batch(&removal(vec![locationless(track(9))])))
            .contains("outstanding insert batch requires settlement")
    );
    // Start is durable and hands out the one permit.
    let permit = j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    assert_eq!(permit.operation(), &[5; 40]);
    assert_eq!(permit.signature(), &[1; 21]);
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.state, InsertBatchState::Started);
    assert_eq!(batch.ready_receipt, Some([8; 32]));
    assert_eq!(batch.signature, Some([1; 21]));
    assert!(
        failure(j.start_insert_batch(id, [8; 32], [1; 21])).contains("already started or closed")
    );
    assert!(failure(j.cancel_insert_batch(id)).contains("only a Prepared"));
    // A reopened journal still holds the Started batch and never re-issues a permit.
    drop(j);
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    assert_eq!(
        j.outstanding_insert_batch().unwrap().unwrap().state,
        InsertBatchState::Started
    );
    assert!(
        failure(j.start_insert_batch(id, [8; 32], [1; 21])).contains("already started or closed")
    );
    // Settlement binds every target.
    let bound = claims(&j, id, &[0, 1, 2]);
    assert_eq!(
        j.settle_insert_batch(id, [7; 32], 14, &bound, &[]).unwrap(),
        InsertBatchState::Verified
    );
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.state, InsertBatchState::Verified);
    assert_eq!(batch.result_receipt, Some([7; 32]));
    assert_eq!(batch.verified_anchor, Some(14));
    assert!(
        batch
            .targets
            .iter()
            .all(|t| t.outcome == Some(TargetOutcome::Bound))
    );
    idle_clean(&j);
    assert_eq!(j.active_bindings().unwrap().len(), 3);
    for (ordinal, target) in batch.targets.iter().enumerate() {
        let active = j
            .lookup_active(&target.target.content.source.audio)
            .unwrap()
            .unwrap();
        assert_eq!(target.binding, Some((active.reference.id, 1)), "{ordinal}");
        assert_eq!(
            active.binding.receipt_kind,
            BindingReceiptKind::InsertBatchVerification
        );
        assert_eq!(active.binding.receipt_sha256, [9; 32]);
        assert_eq!(active.binding.media_anchor, 14);
        assert!(active.binding.content == target.target.content);
    }
    let (provenance, kinds): (i64, i64) = j
        .connection
        .query_row(
            "SELECT (SELECT count(*) FROM binding_records WHERE provenance=2 AND first_attempt IS NULL),(SELECT count(*) FROM binding_versions WHERE receipt_kind=3 AND attempt IS NULL)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((provenance, kinds), (3, 3));
    // Every reservation is consumed and still named by the batch.
    let after = reserved(&j);
    assert_eq!(after.len(), 8);
    assert!(after.iter().all(|r| r.3 && r.4 == Some(id.as_i64())));
    assert_eq!(j.insert_batch_counts().unwrap().verified, 1);
    // The identities never return: even a fresh Insert of the same track refuses.
    assert!(failure(try_prepare_insert(&mut j, alac_content(5))).contains("already bound"));
    // Ordinary writes resume.
    insert_verified(&mut j, alac_content(9));
    // A later removal batch retires a bound target like any owned binding.
    let victim = batch.targets[1].target.content.clone();
    let removal_batch = j
        .prepare_removal_batch(&removal(vec![owned(&j, &victim)]))
        .unwrap();
    j.start_removal_batch(removal_batch, [8; 32], [1; 21])
        .unwrap();
    j.settle_removal_batch(removal_batch, [6; 32], 15).unwrap();
    assert_eq!(j.active_bindings().unwrap().len(), 3);
    assert!(j.lookup_active(&victim.source.audio).unwrap().is_none());
    let tombstones: i64 = j
        .connection
        .query_row(
            "SELECT count(*) FROM binding_tombstones WHERE batch IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tombstones, 1);
    // The batch itself is history: its target still names the retired binding.
    assert_eq!(j.insert_batch(id).unwrap(), batch);
    assert_eq!(j.insert_batches().unwrap().len(), 1);
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
    Journal::open_read_only(&t.path(), &identity()).unwrap();
}

/// A reset removes a bound track and later inserts the same audio again under
/// a new track and path: the retired binding stays history and a new active
/// binding is made.
#[test]
fn retired_audio_is_inserted_again_by_a_later_batch() {
    let t = Temp::new();
    let mut j = created(&t);
    let first = verified(&mut j, &[1, 2]);
    let victim = j.insert_batch(first).unwrap().targets[0]
        .target
        .content
        .clone();
    assert_eq!(
        j.bound_insert_targets().unwrap().get(&victim.track_pid),
        Some(&(first, 0))
    );
    let removal_batch = j
        .prepare_removal_batch(&removal(vec![owned(&j, &victim)]))
        .unwrap();
    j.start_removal_batch(removal_batch, [8; 32], [1; 21])
        .unwrap();
    j.settle_removal_batch(removal_batch, [6; 32], 15).unwrap();
    assert!(j.lookup_active(&victim.source.audio).unwrap().is_none());
    // A retired binding no longer names its batch.
    assert!(
        !j.bound_insert_targets()
            .unwrap()
            .contains_key(&victim.track_pid)
    );
    let mut again = victim.clone();
    again.track_pid = 0x7777_0000_0000_0001;
    again.path = "iTunes_Control/Music/F00/READ.m4a".into();
    let mut request = intent(&[3]);
    request.base_anchor = 15;
    request.targets = vec![InsertTarget {
        content: again.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        conversion: None,
        source: None,
    }];
    let id = j.prepare_insert_batch(&request).unwrap();
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let bound = claims_at(&j, id, &[0], 16);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 16, &bound, &[]).unwrap(),
        InsertBatchState::Verified
    );
    let active = j.lookup_active(&again.source.audio).unwrap().unwrap();
    assert!(active.binding.content == again);
    assert_eq!(
        j.bound_insert_targets().unwrap().get(&again.track_pid),
        Some(&(id, 0))
    );
    // The removed track and path stay reserved by the removal.
    let mut reused = again.clone();
    reused.track_pid = victim.track_pid;
    reused.path = "iTunes_Control/Music/F00/REUS.m4a".into();
    request.targets[0].content = reused;
    assert!(j.prepare_insert_batch(&request).is_err());
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

fn conversion(seed: u8) -> Conversion {
    Conversion {
        source_path: format!("/music/{seed}.flac").into(),
        source_bytes: 900,
        source_sha256: [seed; 32],
        source_audio: [seed.wrapping_add(100); 32],
        ffmpeg: "ffmpeg version 8.1.3 Copyright (c) 2000-2026 the FFmpeg developers".into(),
    }
}

#[test]
fn a_converted_target_links_its_flac_to_the_track_until_the_track_is_removed() {
    let t = Temp::new();
    let mut j = created(&t);
    let mut request = intent(&[1, 2]);
    request.targets[0].conversion = Some(conversion(1));
    let id = j.prepare_insert_batch(&request).unwrap();
    assert_eq!(rows(&j.connection, "insert_conversions"), 1);
    // Only a settled target's binding is a link.
    assert!(j.conversions().unwrap().is_empty());
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let bound = claims(&j, id, &[0, 1]);
    j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap();
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.targets[0].target.conversion, Some(conversion(1)));
    assert_eq!(batch.targets[1].target.conversion, None);
    assert_eq!(
        j.conversions().unwrap(),
        BTreeMap::from([(track(1), conversion(1))])
    );
    // The same FLAC is never converted twice, nor twice in one batch.
    let mut again = intent(&[3]);
    again.base_anchor = 15;
    again.targets[0].conversion = Some(conversion(1));
    assert!(failure(j.prepare_insert_batch(&again)).contains("already on the phone"));
    let mut twice = intent(&[3, 4]);
    twice.base_anchor = 15;
    twice.targets[0].conversion = Some(conversion(5));
    twice.targets[1].conversion = Some(conversion(5));
    assert!(failure(j.prepare_insert_batch(&twice)).contains("same audio twice"));
    let mut relative = intent(&[3]);
    relative.targets[0].conversion = Some(Conversion {
        source_path: "music/3.flac".into(),
        ..conversion(3)
    });
    assert!(failure(j.prepare_insert_batch(&relative)).contains("absolute"));
    // History is immutable, and a link is written only with its Prepared target.
    for sql in [
        "UPDATE insert_conversions SET ffmpeg='other'",
        "DELETE FROM insert_conversions",
    ] {
        assert!(sql_error(&j, sql).contains("immutable"), "{sql}");
    }
    assert!(
        sql_error(
            &j,
            &format!(
                "INSERT INTO insert_conversions VALUES({},1,'/m.flac',1,zeroblob(32),zeroblob(32),'f')",
                id.as_i64()
            )
        )
        .contains("Prepared insert target")
    );
    drop(j);
    let read = Journal::open_read_only(&t.path(), &identity()).unwrap();
    assert_eq!(read.conversions().unwrap().len(), 1);
    drop(read);
    // Removing the track ends the link; the FLAC may then be converted again.
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    let content = batch.targets[0].target.content.clone();
    let removal_batch = j
        .prepare_removal_batch(&removal(vec![owned(&j, &content)]))
        .unwrap();
    j.start_removal_batch(removal_batch, [8; 32], [1; 21])
        .unwrap();
    j.settle_removal_batch(removal_batch, [6; 32], 15).unwrap();
    assert!(j.conversions().unwrap().is_empty());
    let mut readd = intent(&[3]);
    readd.base_anchor = 15;
    readd.targets[0].conversion = Some(conversion(1));
    let id = j.prepare_insert_batch(&readd).unwrap();
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let bound = claims_at(&j, id, &[0], 16);
    j.settle_insert_batch(id, [9; 32], 16, &bound, &[]).unwrap();
    assert_eq!(
        j.conversions().unwrap(),
        BTreeMap::from([(track(3), conversion(1))])
    );
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn partial_insert_batch_blocks_writes_until_a_compensating_removal_settles_it() {
    let t = Temp::new();
    let mut j = created(&t);
    let id = partial(&mut j);
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.state, InsertBatchState::Partial);
    let outcomes: Vec<_> = batch.targets.iter().map(|t| t.outcome).collect();
    assert_eq!(
        outcomes,
        [
            Some(TargetOutcome::Bound),
            Some(TargetOutcome::PendingCompensation),
            Some(TargetOutcome::Bound)
        ]
    );
    assert!(batch.targets[1].binding.is_none());
    assert_eq!(j.active_bindings().unwrap().len(), 2);
    // Only bound targets name their batch.
    assert_eq!(
        j.bound_insert_targets().unwrap(),
        BTreeMap::from([(track(4), (id, 0)), (track(6), (id, 2))])
    );
    assert_eq!(j.insert_batch_counts().unwrap().partial, 1);
    // Partial stays outstanding and keeps the journal for itself.
    assert_eq!(j.outstanding_insert_batch().unwrap().unwrap().id, id);
    assert!(
        failure(try_prepare_insert(&mut j, alac_content(9))).contains("outstanding insert batch")
    );
    assert!(failure(j.prepare_insert_batch(&intent(&[9]))).contains("outstanding insert batch"));
    assert!(
        failure(j.begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 14))
            .contains("outstanding insert batch")
    );
    assert!(
        failure(j.prepare_removal_batch(&removal(vec![locationless(track(9))])))
            .contains("outstanding insert batch")
    );
    assert!(failure(j.cancel_insert_batch(id)).contains("only a Prepared"));
    assert!(failure(j.mark_insert_batch_not_applied(id, [4; 32])).contains("only a Started"));
    assert!(failure(j.settle_insert_batch(id, [4; 32], 15, &[], &[])).contains("not Started"));
    // The reservations of the bound targets are consumed, the missing one's are not.
    let consumed = |j: &Journal, seed: u8| -> bool {
        j.connection
            .query_row(
                "SELECT consumed FROM reservations WHERE namespace=1 AND value=?1",
                [track(seed).to_be_bytes()],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(consumed(&j, 4) && !consumed(&j, 5) && consumed(&j, 6));
    // Wrong compensations refuse: owned, with a path, another batch's, empty, both kinds.
    let bound_track = batch.targets[0].target.content.clone();
    let wrong_owned = {
        let mut request = removal(vec![owned(&j, &bound_track)]);
        request.compensates_insert_batch = Some(id);
        request
    };
    let error = failure(j.prepare_removal_batch(&wrong_owned));
    assert!(error.contains("locationless unowned"), "{error}");
    let mut with_path = compensation(id, &[track(5)]);
    with_path.targets[0].path = Some("iTunes_Control/Music/F40/U001.mp3".into());
    with_path.targets[0].file_bytes = Some(900);
    with_path.targets[0].file_sha256 = Some([55; 32]);
    assert!(failure(j.prepare_removal_batch(&with_path)).contains("locationless unowned"));
    let other = failure(j.prepare_removal_batch(&compensation(id, &[track(9)])));
    assert!(other.contains("exactly the locationless rows"), "{other}");
    let too_many = failure(j.prepare_removal_batch(&compensation(id, &[track(5), track(9)])));
    assert!(
        too_many.contains("exactly the locationless rows"),
        "{too_many}"
    );
    assert!(failure(j.prepare_removal_batch(&compensation(id, &[]))).contains("between one and"));
    let mut both = compensation(id, &[track(5)]);
    both.compensates = Some(AttemptId(1));
    assert!(failure(j.prepare_removal_batch(&both)).contains("not both"));
    let missing = compensation(InsertBatchId(id.as_i64() + 7), &[track(5)]);
    assert!(failure(j.prepare_removal_batch(&missing)).contains("only outstanding insert batch"));
    assert_eq!(rows(&j.connection, "removal_batches"), 0);
    // The exact compensation is admitted; a cancelled one may be prepared again.
    let first = j
        .prepare_removal_batch(&compensation(id, &[track(5)]))
        .unwrap();
    let record = j.removal_batch(first).unwrap();
    assert_eq!(record.compensates_insert_batch, Some(id));
    assert_eq!(record.compensates, None);
    assert_eq!(j.outstanding_removal_batch().unwrap().unwrap().id, first);
    assert!(
        failure(j.prepare_removal_batch(&compensation(id, &[track(5)])))
            .contains("outstanding removal batch")
    );
    j.cancel_removal_batch(first).unwrap();
    let second = j
        .prepare_removal_batch(&compensation(id, &[track(5)]))
        .unwrap();
    j.start_removal_batch(second, [8; 32], [1; 21]).unwrap();
    // A sent compensation proven unapplied leaves the insert batch Partial.
    j.mark_removal_batch_not_applied(second, [4; 32]).unwrap();
    assert_eq!(j.insert_batch(id).unwrap().state, InsertBatchState::Partial);
    let third = j
        .prepare_removal_batch(&compensation(id, &[track(5)]))
        .unwrap();
    j.start_removal_batch(third, [8; 32], [1; 21]).unwrap();
    assert!(failure(try_prepare_insert(&mut j, alac_content(9))).contains("outstanding"));
    j.settle_removal_batch(third, [6; 32], 15).unwrap();
    // The insert batch is Compensated, the row's target too, and the journal idle.
    let batch = j.insert_batch(id).unwrap();
    assert_eq!(batch.state, InsertBatchState::Compensated);
    let outcomes: Vec<_> = batch.targets.iter().map(|t| t.outcome).collect();
    assert_eq!(
        outcomes,
        [
            Some(TargetOutcome::Bound),
            Some(TargetOutcome::Compensated),
            Some(TargetOutcome::Bound)
        ]
    );
    assert_eq!(j.insert_batch_counts().unwrap().compensated, 1);
    idle_clean(&j);
    assert_eq!(j.active_bindings().unwrap().len(), 2);
    // The compensated row's identity is gone for good, ours stay bound.
    let error = failure(try_prepare_insert(&mut j, alac_content(5)));
    assert!(
        error.contains("removed catalogue row") || error.contains("reserved"),
        "{error}"
    );
    insert_verified(&mut j, alac_content(9));
    drop(j);
    let j = Journal::open(&t.path(), &identity()).unwrap();
    assert_eq!(j.insert_batch(id).unwrap(), batch);
    Journal::open_read_only(&t.path(), &identity()).unwrap();
}

#[test]
fn compensation_of_an_insert_batch_needs_a_partial_one_and_leaves_attempt_compensation_alone() {
    // A batch that is not Partial is not compensated.
    let t = Temp::new();
    let mut j = created(&t);
    let id = started(&mut j, &[4, 5]);
    assert!(
        failure(j.prepare_removal_batch(&compensation(id, &[track(4)])))
            .contains("Partial insert batch")
    );
    // SQL agrees: neither a Started batch nor a missing one is compensated.
    let sql = format!(
        "INSERT INTO removal_batches(compensates_insert_batch,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
        id.as_i64()
    );
    assert!(sql_error(&j, &sql).contains("requires a Partial insert batch"));
    let bound = claims(&j, id, &[0, 1]);
    j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap();
    let verified_one = j.insert_batch(id).unwrap();
    assert_eq!(verified_one.state, InsertBatchState::Verified);
    assert!(
        failure(j.prepare_removal_batch(&compensation(id, &[track(4)])))
            .contains("only outstanding insert batch")
    );
    drop(j);
    // The removal-batch compensation of a Started Insert keeps working unchanged.
    let t = Temp::new();
    let mut j = created(&t);
    let parent = insert_started(&mut j, alac_content(4));
    let mut request = removal(vec![locationless(track(4))]);
    request.compensates = Some(parent);
    let batch = j.prepare_removal_batch(&request).unwrap();
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(batch, [6; 32], 15).unwrap();
    assert_eq!(j.attempt(parent).unwrap().state, AttemptState::Compensated);
    assert_eq!(j.removal_batch(batch).unwrap().state, BatchState::Verified);
    idle_clean(&j);
    // An insert batch cannot be prepared beside that Started Insert.
    let t2 = Temp::new();
    let mut j2 = created(&t2);
    insert_started(&mut j2, alac_content(4));
    assert!(failure(j2.prepare_insert_batch(&intent(&[5]))).contains("outstanding attempt"));
}

#[test]
fn insert_batch_cancel_not_applied_and_invalid_transitions() {
    let t = Temp::new();
    let mut j = created(&t);
    // Cancelled: only from Prepared, binds nothing, keeps the identities reserved.
    let cancelled = prepared(&mut j, &[4, 5]);
    assert!(
        failure(j.mark_insert_batch_not_applied(cancelled, [4; 32])).contains("only a Started")
    );
    assert!(
        failure(j.settle_insert_batch(cancelled, [4; 32], 14, &[], &[])).contains("not Started")
    );
    j.cancel_insert_batch(cancelled).unwrap();
    assert_eq!(
        j.insert_batch(cancelled).unwrap().state,
        InsertBatchState::Cancelled
    );
    assert!(j.insert_batch(cancelled).unwrap().ready_receipt.is_none());
    assert!(failure(j.cancel_insert_batch(cancelled)).contains("only a Prepared"));
    assert!(
        failure(j.start_insert_batch(cancelled, [8; 32], [1; 21]))
            .contains("already started or closed")
    );
    idle_clean(&j);
    assert!(j.active_bindings().unwrap().is_empty());
    let error = failure(j.prepare_insert_batch(&intent(&[5])));
    assert!(error.contains("already reserved"), "{error}");
    assert!(failure(try_prepare_insert(&mut j, alac_content(4))).contains("already reserved"));
    // NotApplied: only from Started, binds nothing, keeps the identities reserved.
    let sent = started(&mut j, &[6, 7]);
    assert!(
        failure(j.start_insert_batch(sent, [8; 32], [1; 21])).contains("already started or closed")
    );
    assert!(failure(j.mark_insert_batch_not_applied(sent, [0; 32])).contains("digest"));
    j.mark_insert_batch_not_applied(sent, [4; 32]).unwrap();
    let record = j.insert_batch(sent).unwrap();
    assert_eq!(record.state, InsertBatchState::NotApplied);
    assert_eq!(record.result_receipt, Some([4; 32]));
    assert_eq!(record.verified_anchor, None);
    assert!(
        record
            .targets
            .iter()
            .all(|t| t.outcome.is_none() && t.binding.is_none())
    );
    assert!(failure(j.settle_insert_batch(sent, [4; 32], 14, &[], &[])).contains("not Started"));
    assert!(failure(j.mark_insert_batch_not_applied(sent, [4; 32])).contains("only a Started"));
    assert!(failure(j.cancel_insert_batch(sent)).contains("only a Prepared"));
    idle_clean(&j);
    assert!(j.active_bindings().unwrap().is_empty());
    assert!(failure(j.prepare_insert_batch(&intent(&[7]))).contains("already reserved"));
    let counts = j.insert_batch_counts().unwrap();
    assert_eq!((counts.cancelled, counts.not_applied), (1, 1));
    assert_eq!(j.insert_batches().unwrap().len(), 2);
    // The journal is free for a different batch.
    verified(&mut j, &[8, 9]);
    // SQL enforces the same transitions and immutability.
    let ids: Vec<i64> = j
        .insert_batches()
        .unwrap()
        .iter()
        .map(|b| b.id.as_i64())
        .collect();
    let (cancelled_id, sent_id, done_id) = (ids[0], ids[1], ids[2]);
    for (sql, message) in [
        (
            format!("UPDATE insert_batches SET state=2 WHERE id={cancelled_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=1 WHERE id={sent_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=6 WHERE id={done_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET base_anchor=0 WHERE id={done_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET operation=x'02' WHERE id={done_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET coordination=1 WHERE id={done_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET result_receipt=zeroblob(32) WHERE id={done_id}"),
            "insert batch",
        ),
        (
            format!("DELETE FROM insert_batches WHERE id={done_id}"),
            "history is retained",
        ),
        (
            format!("UPDATE insert_targets SET path='x' WHERE batch={done_id}"),
            "preparation is immutable",
        ),
        (
            format!("UPDATE insert_targets SET outcome=2 WHERE batch={done_id}"),
            "invalid insert target outcome",
        ),
        (
            format!("DELETE FROM insert_targets WHERE batch={done_id}"),
            "history is retained",
        ),
        (
            format!(
                "INSERT INTO insert_targets(batch,ordinal,track,path,content) VALUES({done_id},2,zeroblob(8),'q',x'01')"
            ),
            "extends a Prepared batch",
        ),
    ] {
        let error = sql_error(&j, &sql);
        assert!(error.contains(message), "{sql}: {error}");
    }
    // Forward-only transitions on a live row.
    let live = prepared(&mut j, &[10]);
    let live_id = live.as_i64();
    for (sql, message) in [
        (
            format!("UPDATE insert_batches SET state=2 WHERE id={live_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=4 WHERE id={live_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=5 WHERE id={live_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=6 WHERE id={live_id}"),
            "insert batch",
        ),
        (
            format!("UPDATE insert_batches SET state=1 WHERE id={live_id}"),
            "CHECK",
        ),
        (
            format!(
                "UPDATE insert_batches SET ready_receipt=zeroblob(32),signature=zeroblob(21),state=1 WHERE id={live_id}; UPDATE insert_batches SET ready_receipt=zeroblob(32) WHERE id={live_id}"
            ),
            "start is immutable",
        ),
        (
            format!(
                "UPDATE insert_batches SET ready_receipt=zeroblob(32),signature=zeroblob(21),state=1 WHERE id={live_id}; UPDATE insert_batches SET state=2,result_receipt=zeroblob(32),verified_anchor=14 WHERE id={live_id}"
            ),
            "state differs from its target outcomes",
        ),
        (
            format!(
                "UPDATE insert_batches SET ready_receipt=zeroblob(32),signature=zeroblob(21),state=1 WHERE id={live_id}; UPDATE insert_batches SET state=5,result_receipt=zeroblob(32),verified_anchor=14 WHERE id={live_id}"
            ),
            "state differs from its target outcomes",
        ),
    ] {
        let error = sql_error(&j, &sql);
        assert!(error.contains(message), "{sql}: {error}");
    }
    j.cancel_insert_batch(live).unwrap();
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

#[test]
fn insert_batch_refuses_malformed_intents_and_unavailable_identities() {
    let t = Temp::new();
    let mut j = created(&t);
    insert_verified(&mut j, alac_content(1));
    let bad = |j: &mut Journal, edit: &dyn Fn(&mut InsertIntent)| -> String {
        let mut request = intent(&[4, 5]);
        edit(&mut request);
        failure(j.prepare_insert_batch(&request))
    };
    assert!(bad(&mut j, &|i| i.targets.clear()).contains("between one and"));
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
    assert!(bad(&mut j, &|i| i.targets[1] = i.targets[0].clone()).contains("twice"));
    assert!(
        bad(&mut j, &|i| i.targets[1].content.path =
            lower_name(&i.targets[0].content.path))
        .contains("path twice")
    );
    assert!(
        bad(&mut j, &|i| i.targets =
            vec![target(4); coordination::MAX_TARGETS + 1])
        .contains("between one and")
    );
    assert!(
        bad(&mut j, &|i| i.targets[1].content.source =
            alac_content(4).source)
        .contains("same audio twice")
    );
    assert!(bad(&mut j, &|i| i.targets[0].content.track_pid = 0).contains("track identity"));
    assert!(
        bad(&mut j, &|i| i.targets[0].new_artist_pid =
            i.targets[0].new_album_pid)
        .contains("nonzero and distinct")
    );
    assert!(
        bad(&mut j, &|i| i.targets[0].new_album_pid = Some(0)).contains("nonzero and distinct")
    );
    assert!(
        bad(&mut j, &|i| i.targets[0].new_album_pid =
            Some(i.targets[0].content.track_pid))
        .contains("nonzero and distinct")
    );
    assert!(
        bad(&mut j, &|i| i.targets[1].new_artist_pid =
            i.targets[0].new_album_pid)
        .contains("nonzero and distinct")
    );
    assert!(
        bad(&mut j, &|i| i.targets[0].content.path =
            "somewhere/else.m4a".into())
        .contains("permanent music path")
    );
    assert!(
        bad(&mut j, &|i| {
            i.targets[0].content.artwork = Some(cover(100));
            i.targets[1].content.artwork = Some(cover(100));
        })
        .contains("cache ID twice")
    );
    assert!(
        bad(&mut j, &|i| i.targets[0].content.artwork = Some(Artwork {
            cache_id: 99,
            ..cover(100)
        }))
        .contains("invalid artwork reservation")
    );
    assert_eq!(rows(&j.connection, "insert_batches"), 0);
    // Encoded audio, tracks and paths that already belong to binding history.
    let bound = alac_content(1);
    let mut again = intent(&[4]);
    again.targets[0].content = bound.clone();
    assert!(failure(j.prepare_insert_batch(&again)).contains("already bound"));
    let mut same_track = intent(&[4]);
    same_track.targets[0].content.track_pid = bound.track_pid;
    assert!(failure(j.prepare_insert_batch(&same_track)).contains("belongs to binding history"));
    let mut same_path = intent(&[4]);
    same_path.targets[0].content.path = lower_name(&bound.path);
    assert!(failure(j.prepare_insert_batch(&same_path)).contains("belongs to binding history"));
    // A new collection id another claim already holds.
    let mut taken = intent(&[4]);
    taken.targets[0].new_album_pid = Some(bound.track_pid);
    assert!(failure(j.prepare_insert_batch(&taken)).contains("already reserved"));
    assert_eq!(rows(&j.connection, "insert_batches"), 0);
    // Nothing a refused call tried survives: the reservations are as before.
    assert_eq!(
        rows(&j.connection, "reservations WHERE insert_batch IS NOT NULL"),
        0
    );
    // Rows a removal batch names are gone for good.
    let t2 = Temp::new();
    let mut k = created(&t2);
    let victim = alac_content(4);
    insert_verified(&mut k, victim.clone());
    let batch = k
        .prepare_removal_batch(&removal(vec![owned(&k, &victim)]))
        .unwrap();
    k.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    k.settle_removal_batch(batch, [6; 32], 15).unwrap();
    let mut revived = intent(&[5]);
    revived.targets[0].content.track_pid = victim.track_pid;
    revived.targets[0].new_album_pid = None;
    revived.targets[0].new_artist_pid = None;
    assert!(failure(k.prepare_insert_batch(&revived)).contains("belongs to binding history"));
    // Removed catalogue rows that never had a binding stay unavailable as well.
    let unowned = RemovalTarget {
        track: track(8),
        item_pid: 1,
        path: Some(alac_content(8).path),
        file_bytes: Some(900),
        file_sha256: Some([55; 32]),
        binding: None,
    };
    let batch = k.prepare_removal_batch(&removal(vec![unowned])).unwrap();
    k.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    k.settle_removal_batch(batch, [6; 32], 16).unwrap();
    let error = failure(k.prepare_insert_batch(&intent(&[8])));
    assert!(error.contains("removed catalogue row"), "{error}");
}

#[test]
fn settlement_checks_every_claim_and_is_all_or_nothing() {
    let t = Temp::new();
    let mut j = created(&t);
    let id = started(&mut j, &[4, 5, 6]);
    let good = claims(&j, id, &[0, 1, 2]);
    let with = |edit: &dyn Fn(&mut Vec<(usize, CompletionEvidence)>)| {
        let mut claims = claims(&j, id, &[0, 1, 2]);
        edit(&mut claims);
        claims
    };
    for (claims, message) in [
        (with(&|c| c[1].1.track_pid += 1), "verified content differs"),
        (
            with(&|c| c[1].1.path = "iTunes_Control/Music/F39/AAAZ.m4a".into()),
            "verified content differs",
        ),
        (
            with(&|c| c[2].1.audio_bytes += 1),
            "verified content differs",
        ),
        (
            with(&|c| c[2].1.audio_file_sha256 = [0xEE; 32]),
            "verified content differs",
        ),
        (
            with(&|c| c[0].1.artwork_sha256 = Some([5; 32])),
            "verified content differs",
        ),
        (with(&|c| c[1].1.receipt_sha256 = [0; 32]), "digest"),
        (with(&|c| c[2].1.media_anchor = 15), "anchor differs"),
        (with(&|c| c[1].0 = 7), "outside the batch"),
        (with(&|c| c[1].0 = 0), "twice"),
    ] {
        let error = failure(j.settle_insert_batch(id, [9; 32], 14, &claims, &[]));
        assert!(error.contains(message), "{message}: {error}");
    }
    // Digest, anchor and state refusals.
    assert!(failure(j.settle_insert_batch(id, [0; 32], 14, &good, &[])).contains("digest"));
    assert!(failure(j.settle_insert_batch(id, [9; 32], 13, &good, &[])).contains("does not cover"));
    assert!(failure(j.settle_insert_batch(id, [9; 32], 0, &good, &[])).contains("does not cover"));
    // Nothing a refused call did survives: still Started, no bindings, nothing consumed.
    assert_eq!(j.insert_batch(id).unwrap().state, InsertBatchState::Started);
    assert!(j.active_bindings().unwrap().is_empty());
    assert_eq!(rows(&j.connection, "binding_records"), 0);
    assert_eq!(rows(&j.connection, "reservations WHERE consumed=1"), 0);
    assert_eq!(
        rows(&j.connection, "insert_targets WHERE outcome IS NOT NULL"),
        0
    );
    // A later anchor than the batch's revision is a legitimate reconciliation.
    let late = claims_at(&j, id, &[0, 1, 2], 20);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 20, &late, &[]).unwrap(),
        InsertBatchState::Verified
    );
    assert_eq!(j.insert_batch(id).unwrap().verified_anchor, Some(20));
    // No claim at all: every row is a catalogue row without its file.
    let mut request = intent(&[7, 8]);
    request.base_anchor = 20;
    let none = j.prepare_insert_batch(&request).unwrap();
    j.start_insert_batch(none, [8; 32], [1; 21]).unwrap();
    assert_eq!(
        j.settle_insert_batch(none, [9; 32], 21, &[], &[]).unwrap(),
        InsertBatchState::Partial
    );
    let record = j.insert_batch(none).unwrap();
    assert!(
        record
            .targets
            .iter()
            .all(|t| t.outcome == Some(TargetOutcome::PendingCompensation))
    );
    assert!(
        j.lookup_active(&alac_content(7).source.audio)
            .unwrap()
            .is_none()
    );
}
/// Covers ride in the batch: each reserves its cache ID for the batch, a bound
/// target consumes it, and a target left without audio keeps it reserved so
/// the ID is never offered again.
#[test]
fn covered_targets_reserve_their_cache_ids_and_bind_their_artwork() {
    let t = Temp::new();
    let mut j = created(&t);
    let mut request = intent(&[4, 5, 6]);
    request.targets[0].content.artwork = Some(cover(140));
    request.targets[2].content.artwork = Some(cover(141));
    let id = j.prepare_insert_batch(&request).unwrap();
    assert_eq!(j.reserved_cache_ids().unwrap(), BTreeSet::from([140, 141]));
    // Another batch cannot take a reserved cache ID.
    let mut other = intent(&[7]);
    other.targets[0].content.artwork = Some(cover(141));
    j.cancel_insert_batch(id).unwrap();
    assert!(failure(j.prepare_insert_batch(&other)).contains("already reserved"));
    let id = {
        let mut again = intent(&[8, 9, 10]);
        again.targets[0].content.artwork = Some(cover(150));
        again.targets[2].content.artwork = Some(cover(151));
        j.prepare_insert_batch(&again).unwrap()
    };
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    // A claim must carry the planned cover's digest, or none for no cover.
    let mut refused = claims(&j, id, &[0, 2]);
    refused[1].1.artwork_sha256 = None;
    assert!(
        failure(j.settle_insert_batch(id, [9; 32], 14, &refused, &[]))
            .contains("verified content differs")
    );
    let good = claims(&j, id, &[0, 2]);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &good, &[]).unwrap(),
        InsertBatchState::Partial
    );
    let consumed = |cache: u32| -> Option<bool> {
        j.connection
            .query_row(
                "SELECT consumed FROM reservations WHERE namespace=2 AND value=?1 AND insert_batch=?2",
                params![cache.to_be_bytes(), id.as_i64()],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(consumed(150), Some(true));
    assert_eq!(consumed(151), Some(true));
    let bound = j
        .lookup_active(&alac_content(8).source.audio)
        .unwrap()
        .unwrap();
    assert!(bound.binding.content.artwork == Some(cover(150)));
    // The middle target had no cover; it awaits compensation.
    let record = j.insert_batch(id).unwrap();
    assert_eq!(
        record.targets[1].outcome,
        Some(TargetOutcome::PendingCompensation)
    );
    // Reopening validates the covered reservations.
    drop(j);
    let j = Journal::open(&t.path(), &identity()).unwrap();
    assert_eq!(
        j.reserved_cache_ids().unwrap(),
        BTreeSet::from([140, 141, 150, 151])
    );
}

// --- native writes exclude each other --------------------------------------------

#[test]
fn native_writes_exclude_each_other_in_rust_and_sql() {
    // An attempt blocks an insert batch, in Rust and in SQL, and vice versa.
    let t = Temp::new();
    let mut j = created(&t);
    insert_started(&mut j, alac_content(1));
    assert!(failure(j.prepare_insert_batch(&intent(&[4]))).contains("outstanding attempt"));
    assert!(
        sql_error(&j, RAW_INSERT_BATCH).contains("an insert batch is Prepared on an idle journal")
    );
    drop(j);
    let t = Temp::new();
    let mut j = created(&t);
    let attempt = prepare_insert(&mut j, alac_content(1));
    assert!(failure(j.prepare_insert_batch(&intent(&[4]))).contains("outstanding attempt"));
    assert!(
        sql_error(&j, RAW_INSERT_BATCH).contains("an insert batch is Prepared on an idle journal")
    );
    j.cancel_prepared(attempt).unwrap();
    // A removal batch blocks it as well.
    let content = alac_content(2);
    insert_verified(&mut j, content.clone());
    let batch = j
        .prepare_removal_batch(&removal(vec![owned(&j, &content)]))
        .unwrap();
    assert!(failure(j.prepare_insert_batch(&intent(&[4]))).contains("outstanding removal batch"));
    assert!(
        sql_error(&j, RAW_INSERT_BATCH).contains("an insert batch is Prepared on an idle journal")
    );
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    assert!(failure(j.prepare_insert_batch(&intent(&[4]))).contains("outstanding removal batch"));
    j.settle_removal_batch(batch, [6; 32], 15).unwrap();
    // An outstanding insert batch blocks an attempt, a removal batch, a compensation,
    // a metadata batch and a second insert batch, in Rust and in SQL.
    let id = prepared(&mut j, &[4, 5]);
    assert!(
        failure(try_prepare_insert(&mut j, alac_content(9)))
            .contains("outstanding insert batch requires settlement")
    );
    assert!(sql_error(&j, RAW_ATTEMPT).contains("an outstanding insert batch requires settlement"));
    assert!(
        failure(j.prepare_removal_batch(&removal(vec![locationless(track(9))])))
            .contains("outstanding insert batch requires settlement")
    );
    assert!(
        sql_error(&j, RAW_REMOVAL_BATCH).contains("a removal batch is Prepared on an idle journal")
    );
    assert!(
        failure(j.prepare_metadata_batch(&metadata_intent(&[1])))
            .contains("outstanding insert batch requires settlement")
    );
    assert!(
        sql_error(&j, RAW_METADATA_BATCH)
            .contains("a metadata batch is Prepared on an idle journal")
    );
    assert!(
        failure(j.prepare_insert_batch(&intent(&[9])))
            .contains("outstanding insert batch requires settlement")
    );
    let second = sql_error(&j, RAW_INSERT_BATCH);
    assert!(
        second.contains("an insert batch is Prepared on an idle journal"),
        "{second}"
    );
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    assert!(
        failure(j.prepare_removal_batch(&removal(vec![locationless(track(9))])))
            .contains("outstanding insert batch requires settlement")
    );
    // Only the compensation of a Partial batch passes; a Started one does not.
    let sql = format!(
        "INSERT INTO removal_batches(compensates_insert_batch,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES({},0,x'01',zeroblob(32),0,zeroblob(32),zeroblob(32),zeroblob(32),'/x')",
        id.as_i64()
    );
    assert!(sql_error(&j, &sql).contains("Partial insert batch"));
    let bound = claims(&j, id, &[0]);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap(),
        InsertBatchState::Partial
    );
    // Even Partial keeps attempts and unrelated removals out, in SQL too.
    assert!(sql_error(&j, RAW_ATTEMPT).contains("an outstanding insert batch requires settlement"));
    assert!(
        sql_error(&j, RAW_REMOVAL_BATCH).contains("a removal batch is Prepared on an idle journal")
    );
    // The one Partial batch admits exactly its own compensation and nothing else.
    let comp = j
        .prepare_removal_batch(&compensation(id, &[track(5)]))
        .unwrap();
    assert!(sql_error(&j, RAW_ATTEMPT).contains("outstanding"));
    assert!(sql_error(&j, &sql).contains("UNIQUE"));
    j.cancel_removal_batch(comp).unwrap();
    let comp = j
        .prepare_removal_batch(&compensation(id, &[track(5)]))
        .unwrap();
    j.start_removal_batch(comp, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(comp, [6; 32], 15).unwrap();
    idle_clean(&j);
    // Settled, everything is admitted again.
    insert_verified(&mut j, alac_content(9));
    verified(&mut j, &[10]);
}

// --- coordination -------------------------------------------------------------------

#[test]
fn coordinated_insert_batch_links_its_step_and_needs_an_active_coordination() {
    let t = Temp::new();
    let mut j = created(&t);
    let c = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 13)
        .unwrap();
    // The coordination owns the journal: an unowned batch is refused without its permit.
    let error = failure(j.prepare_insert_batch(&intent(&[4])));
    assert!(error.contains("owns the journal"), "{error}");
    // A batch may only extend the Active coordination it names.
    let mut stray = intent(&[4]);
    stray.coordination = Some(CoordinationId(c.as_i64() + 5));
    assert!(failure(j.prepare_insert_batch(&stray)).contains("not Active"));
    assert_eq!(rows(&j.connection, "insert_batches"), 0);
    let mut request = intent(&[4, 5]);
    request.coordination = Some(c);
    let id = j.prepare_insert_batch(&request).unwrap();
    assert_eq!(j.insert_batch(id).unwrap().coordination, Some(c));
    assert_eq!(
        j.coordination_steps(c).unwrap(),
        [CoordinationStep {
            step: 0,
            batch: None,
            attempt: None,
            insert_batch: Some(id),
            metadata_batch: None,
        }]
    );
    // It blocks closing the coordination while outstanding, Partial included.
    assert!(failure(j.complete_coordination(c, 0, b"done")).contains("outstanding insert batch"));
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let bound = claims(&j, id, &[0]);
    assert_eq!(
        j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap(),
        InsertBatchState::Partial
    );
    assert!(failure(j.complete_coordination(c, 0, b"done")).contains("outstanding insert batch"));
    // The compensation is the coordination's next step and Verified closes it out.
    let mut comp = compensation(id, &[track(5)]);
    comp.coordination = Some(c);
    let batch = j.prepare_removal_batch(&comp).unwrap();
    let steps = j.coordination_steps(c).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1].batch, Some(batch));
    j.start_removal_batch(batch, [8; 32], [1; 21]).unwrap();
    j.settle_removal_batch(batch, [6; 32], 15).unwrap();
    assert_eq!(
        j.insert_batch(id).unwrap().state,
        InsertBatchState::Compensated
    );
    j.complete_coordination(c, 0, b"done").unwrap();
    // Closed, the steps stay as history; a closed coordination takes no more.
    assert_eq!(j.coordination_steps(c).unwrap().len(), 2);
    let mut late = intent(&[6]);
    late.coordination = Some(c);
    assert!(failure(j.prepare_insert_batch(&late)).contains("not Active"));
    drop(j);
    // A second coordination: the same batch cannot be linked twice.
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    let c2 = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/y"), 15)
        .unwrap();
    // The SQL of the step table: exactly one link, each once, in order.
    for (sql, message) in [
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step) VALUES({},0)",
                c2.as_i64()
            ),
            "CHECK",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,batch,insert_batch) VALUES({},0,1,{})",
                c2.as_i64(),
                id.as_i64()
            ),
            "CHECK",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,insert_batch,metadata_batch) VALUES({},0,{},1)",
                c2.as_i64(),
                id.as_i64()
            ),
            "CHECK",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,insert_batch) VALUES({},1,{})",
                c2.as_i64(),
                id.as_i64()
            ),
            "extends an Active coordination in order",
        ),
        (
            format!(
                "INSERT INTO coordination_steps(coordination,step,insert_batch) VALUES({},0,{})",
                c2.as_i64(),
                id.as_i64()
            ),
            "UNIQUE",
        ),
        (
            format!(
                "UPDATE coordination_steps SET insert_batch=NULL WHERE insert_batch={}",
                id.as_i64()
            ),
            "immutable",
        ),
        ("DELETE FROM coordination_steps".to_string(), "retained"),
    ] {
        let error = sql_error(&j, &sql);
        assert!(error.contains(message), "{sql}: {error}");
    }
    // A Cancelled batch of a closed coordination is likewise retained history.
    let mut request = intent(&[7]);
    request.coordination = Some(c2);
    let cancelled = j.prepare_insert_batch(&request).unwrap();
    j.cancel_insert_batch(cancelled).unwrap();
    j.complete_coordination(c2, 0, b"done").unwrap();
    drop(j);
    Journal::open(&t.path(), &identity()).unwrap();
}

// --- the binding and reservation tables -------------------------------------------------

#[test]
fn binding_provenances_two_and_three_and_receipt_kinds_three_and_four_are_checked() {
    let t = Temp::new();
    let j = created(&t);
    let record = |first: &str, provenance: i64| {
        format!(
            "INSERT INTO binding_records(audio,track,path,first_attempt,provenance) VALUES(zeroblob(32),zeroblob(8),'p',{first},{provenance})"
        )
    };
    // Provenance 1 has its attempt; a batch's 2 and a pull's 3 have none; 4
    // does not exist.
    sql_error(&j, &record("NULL", 1));
    sql_error(&j, &record("1", 2));
    sql_error(&j, &record("1", 3));
    sql_error(&j, &record("NULL", 4));
    let version = |kind: i64, attempt: &str| {
        format!(
            "INSERT INTO binding_versions VALUES(last_insert_rowid(),1,x'01',zeroblob(32),14,{attempt},{kind})"
        )
    };
    // The valid shapes insert (and are rolled back by the probe).
    for (provenance, kind) in [(2, 3), (3, 4)] {
        j.connection.execute_batch("SAVEPOINT ok").unwrap();
        j.connection
            .execute_batch(&record("NULL", provenance))
            .unwrap();
        j.connection.execute_batch(&version(kind, "NULL")).unwrap();
        j.connection
            .execute_batch("ROLLBACK TO ok; RELEASE ok")
            .unwrap();
    }
    // Receipt kinds 3 and 4 never have an attempt; 0 and 5 are not kinds.
    for (kind, attempt) in [(3, "1"), (4, "1"), (5, "NULL"), (0, "NULL")] {
        let sql = format!("{}; {}", record("NULL", 2), version(kind, attempt));
        assert!(sql_error(&j, &sql).contains("CHECK"), "{kind}");
    }
    // The binding tables are immutable.
    drop(j);
    let mut j = Journal::open(&t.path(), &identity()).unwrap();
    verified(&mut j, &[4]);
    for sql in [
        "UPDATE binding_records SET provenance=2",
        "DELETE FROM binding_records",
        "UPDATE binding_versions SET anchor=1",
        "DELETE FROM binding_versions",
    ] {
        assert!(sql_error(&j, sql).contains("immutable"), "{sql}");
    }
    // The reservation owner is one of attempt and insert batch.
    assert!(
        sql_error(
            &j,
            "UPDATE reservations SET attempt=1 WHERE insert_batch IS NOT NULL"
        )
        .contains("CHECK")
    );
    // A provenance-2 binding without its bound insert target fails validation.
    let forged = Temp::new();
    let k = created(&forged);
    let content = alac_content(4);
    k.connection
        .execute_batch(&format!(
            "INSERT INTO binding_records(audio,track,path,first_attempt,provenance) VALUES(x'{}',x'{}','{}',NULL,2); INSERT INTO binding_versions VALUES(last_insert_rowid(),1,x'{}',x'{}',14,NULL,3); INSERT INTO active_bindings VALUES(x'{}',last_insert_rowid(),1);",
            hex(&content.source.audio.sha256),
            hex(&content.track_pid.to_be_bytes()),
            content.path,
            hex(&encode_content(&content).unwrap()),
            hex(&[9; 32]),
            hex(&content.source.audio.sha256),
        ))
        .unwrap();
    let error = failure(validate_schema(&k.connection));
    assert!(error.contains("no bound insert target"), "{error}");
}

#[test]
fn validation_refuses_tampered_batches_targets_and_reservations() {
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
    let setup = |t: &Temp| {
        let mut j = created(t);
        let id = verified(&mut j, &[4, 5]);
        (j, id.as_i64())
    };
    // A Verified batch whose target lost its outcome.
    let t = Temp::new();
    let (j, id) = setup(&t);
    validate_schema(&j.connection).unwrap();
    tamper(
        &j,
        &[
            "insert_target_outcome",
            "insert_target_binding",
            "insert_target_immutable",
        ],
        &format!(
            "UPDATE insert_targets SET outcome=NULL,binding_id=NULL,binding_version=NULL WHERE batch={id} AND ordinal=0"
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(
        error.contains("state differs from its target outcomes"),
        "{error}"
    );
    // A reservation that lost its consumed flag.
    let t = Temp::new();
    let (j, id) = setup(&t);
    j.connection
        .execute_batch(&format!(
            "UPDATE reservations SET consumed=0 WHERE insert_batch={id} AND namespace=3"
        ))
        .unwrap();
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("reservations differ"), "{error}");
    // A reservation that belongs to nothing.
    let t = Temp::new();
    let (j, id) = setup(&t);
    j.connection
        .execute_batch(&format!(
            "UPDATE reservations SET insert_batch=NULL WHERE insert_batch={id} AND namespace=3"
        ))
        .unwrap();
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("reservations differ"), "{error}");
    // A target whose stored content disagrees with its key.
    let t = Temp::new();
    let (j, id) = setup(&t);
    let other = encode_content(&alac_content(9)).unwrap();
    tamper(
        &j,
        &["insert_target_immutable"],
        &format!(
            "UPDATE insert_targets SET content=x'{}' WHERE batch={id} AND ordinal=0",
            other.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("differs from its content"), "{error}");
    // A bound target whose binding is not the one it names.
    let t = Temp::new();
    let (j, id) = setup(&t);
    tamper(
        &j,
        &[
            "insert_target_outcome",
            "insert_target_binding",
            "insert_target_immutable",
        ],
        &format!(
            "UPDATE insert_targets SET binding_id=(SELECT binding_id FROM insert_targets WHERE batch={id} AND ordinal=1),outcome=1 WHERE batch={id} AND ordinal=0"
        ),
    );
    assert!(validate_schema(&j.connection).is_err());
    // A verified anchor that does not cover the revision.
    let t = Temp::new();
    let (j, id) = setup(&t);
    tamper(
        &j,
        &["insert_batch_terminal", "insert_batch_result_immutable"],
        &format!("UPDATE insert_batches SET verified_anchor=13 WHERE id={id}"),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(error.contains("anchor"), "{error}");
    // A coordination column that names a coordination the step table does not.
    let t = Temp::new();
    let (mut j, id) = setup(&t);
    let c = j
        .begin_coordination(CoordinationKind::Clear, b"intent", Path::new("/run/x"), 14)
        .unwrap();
    tamper(
        &j,
        &["insert_batch_terminal", "insert_batch_immutable"],
        &format!(
            "UPDATE insert_batches SET coordination={} WHERE id={id}",
            c.as_i64()
        ),
    );
    let error = failure(validate_schema(&j.connection));
    assert!(
        error.contains("coordination differs from its step"),
        "{error}"
    );
    // An insert batch and an attempt outstanding together.
    let t = Temp::new();
    let mut j = created(&t);
    prepare_insert(&mut j, alac_content(1));
    tamper(&j, &["insert_batch_prepared"], RAW_INSERT_BATCH);
    let error = failure(validate_exclusion(&j.connection));
    assert!(
        error.contains("an insert batch and an attempt are outstanding together"),
        "{error}"
    );
    // An insert batch and a metadata batch outstanding together.
    let t = Temp::new();
    let j = created(&t);
    tamper(&j, &["insert_batch_prepared"], RAW_INSERT_BATCH);
    tamper(&j, &["metadata_batch_prepared"], RAW_METADATA_BATCH);
    let error = failure(validate_exclusion(&j.connection));
    assert!(
        error.contains("insert batch and a metadata batch"),
        "{error}"
    );
}

#[test]
fn a_bound_target_keeps_its_source_path_on_its_binding() {
    let t = Temp::new();
    let mut j = created(&t);
    let mut request = intent(&[4, 5]);
    request.targets[0].source = Some("/music/Artist - One.m4a".into());
    let id = j.prepare_insert_batch(&request).unwrap();
    assert!(
        j.insert_batch(id).unwrap().targets[0].target.source
            == Some("/music/Artist - One.m4a".into())
    );
    assert!(
        j.recorded_sources().unwrap().is_empty(),
        "not before it is bound"
    );
    j.start_insert_batch(id, [8; 32], [1; 21]).unwrap();
    let bound = claims(&j, id, &[0, 1]);
    j.settle_insert_batch(id, [9; 32], 14, &bound, &[]).unwrap();
    let first = request.targets[0].content.track_pid;
    assert_eq!(
        j.recorded_sources().unwrap(),
        BTreeMap::from([(first, PathBuf::from("/music/Artist - One.m4a"))])
    );
    for sql in [
        "UPDATE binding_sources SET source='/x'",
        "DELETE FROM binding_sources",
        "DELETE FROM insert_sources",
    ] {
        assert!(sql_error(&j, sql).contains("immutable"), "{sql}");
    }
}
