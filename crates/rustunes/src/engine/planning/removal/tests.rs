use super::*;
use crate::{
    engine::planning::source_collection::{self, InspectionFailure, SelectionOptions},
    engine::state::audio_index::Absent,
    engine::state::journal::{
        AttemptKind, CompletionEvidence, Content, Identity, Journal, PreparedAttempt, StartEvidence,
    },
};
use music_protocol::media::fingerprint::{EncodedAlac, SourceFingerprint};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "destructive-plan-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
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

fn identities() -> (Identity, IndexIdentity) {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4] = 1;
    raw[6] = 25;
    raw[8] = 1;
    raw[12] = 1;
    raw[20] = 2;
    let identity = Identity::new(
        "destructive-planning-test",
        &music_protocol::catalogue::identity::parse(&raw).unwrap(),
    )
    .unwrap();
    let index = identity.index_identity();
    (identity, index)
}
fn sources(temp: &Temp) -> SourceCollection {
    let root = temp.0.join("source");
    fs::create_dir(&root).unwrap();
    fs::copy(
        crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a"),
        root.join("source.m4a"),
    )
    .unwrap();
    source_collection::inspect(
        &source_collection::select(&root, &SelectionOptions::default()).unwrap(),
    )
    .unwrap()
}
fn fingerprint(seed: u8, bytes: u64) -> SourceFingerprint {
    let mut audio = EncodedAlac {
        sha256: [0; 32],
        alac_config_sha256: [seed; 32],
        sample_sizes_sha256: [2; 32],
        media_sha256: [3; 32],
        timescale: 44100,
        duration_ticks: 132300,
        media_bytes: 800,
    };
    audio.sha256 = audio.composite_sha256();
    SourceFingerprint {
        file_bytes: bytes,
        full_file_sha256: [seed; 32],
        audio,
    }
}
fn receipt(catalog: &Catalog, identity: &IndexIdentity, other: bool) -> Value {
    let h = catalog.source_hashes();
    let mut records = Vec::new();
    let mut others = Vec::new();
    for (n, t) in catalog.tracks().iter().enumerate() {
        let f = fingerprint(u8::try_from(n + 1).unwrap(), t.metadata.file_size);
        let a = &f.audio;
        let attributes =
            json!({"st_ifmt":"S_IFREG","st_size":t.metadata.file_size.to_string(),"st_mtime":"1"});
        if other && n == 0 {
            others.push(json!({"sync_id":format!("{:016X}",t.sync_id),"afc_path":t.afc_path,"attributes":attributes,"file_bytes":f.file_bytes,"full_file_sha256":hex(&f.full_file_sha256),"codec":{"kind":"mpeg1-layer3","sample_rate":44100,"channels":2,"frame_count":10,"audio_offset":0,"audio_bytes":f.file_bytes,"trailing_tag_bytes":0}}));
        } else {
            records.push(json!({"sync_id":format!("{:016X}",t.sync_id),"afc_path":t.afc_path,"attributes":attributes,"fingerprint":{"file_bytes":f.file_bytes,"full_file_sha256":hex(&f.full_file_sha256),"audio":{"sha256":hex(&a.sha256),"alac_config_sha256":hex(&a.alac_config_sha256),"sample_sizes_sha256":hex(&a.sample_sizes_sha256),"media_sha256":hex(&a.media_sha256),"timescale":a.timescale,"duration_ticks":a.duration_ticks,"media_bytes":a.media_bytes}}}));
        }
    }
    json!({"schema":"rustunes.device-audio-index","complete":true,"identity":{"device_identifier_sha256":hex(&identity.device_identifier_sha256),"library_identity_sha256":hex(&identity.library_identity_sha256)},"catalog":{"database_sha256":hex(&h.database),"database_bytes":h.database_bytes,"wal_sha256":h.wal.as_ref().map(hex),"wal_bytes":h.wal_bytes,"persisted_media_anchor":catalog.persisted_media_anchor()},"track_count":catalog.tracks().len(),"total_bytes":catalog.tracks().iter().map(|t|t.metadata.file_size).sum::<u64>(),"alac_track_count":records.len(),"other_track_count":others.len(),"records":records,"other_records":others,"encoded_audio_kind":"encoded-alac-not-pcm","progress_sha256":hex(&[4;32]),"completed_unix_time_ns":"1","fresh_prefs_before_after_components_equal":true,"atomic_snapshot_claim":false,"catalog_metadata":{}})
}
fn index(catalog: &Catalog, identity: &IndexIdentity, other: bool) -> DeviceAudioIndex {
    let temp = Temp::new();
    let path = temp.0.join("index.json");
    let bytes = serde_json::to_vec(&receipt(catalog, identity, other)).unwrap();
    fs::write(&path, &bytes).unwrap();
    DeviceAudioIndex::load_complete(&path, Sha256::digest(bytes).into(), catalog, identity).unwrap()
}
fn content(index: &DeviceAudioIndex, scope: Option<[u8; 32]>) -> Content {
    let a = index.tracks().values().next().unwrap();
    Content {
        source: (a.fingerprint.clone()).into(),
        track_pid: a.sync_id,
        path: a.afc_path.clone(),
        artwork: None,
        collection: scope,
    }
}
fn prepared(content: Content, anchor: u32) -> PreparedAttempt {
    use plist::{Dictionary, Value};
    let dbinfo = Dictionary::from_iter::<[(String, Value); 1]>([(
        "primary_container_pid".into(),
        50_i64.into(),
    )]);
    let db = Dictionary::from_iter::<[(String, Value); 3]>([
        ("operation".into(), "update_db_info".into()),
        ("pid".into(), 51_i64.into()),
        ("db_info".into(), Value::Dictionary(dbinfo)),
    ]);
    let op = Dictionary::from_iter::<[(String, Value); 3]>([
        ("operation".into(), "insert_track".into()),
        ("pid".into(), (content.track_pid as i64).into()),
        ("item".into(), Value::Dictionary(Dictionary::new())),
    ]);
    let envelope = Dictionary::from_iter::<[(String, Value); 3]>([
        ("revision".into(), (anchor + 1).into()),
        (
            "timestamp".into(),
            Value::Date(std::time::UNIX_EPOCH.into()),
        ),
        (
            "operations".into(),
            Value::Array(vec![Value::Dictionary(db), Value::Dictionary(op)]),
        ),
    ]);
    let mut operation = Vec::new();
    Value::Dictionary(envelope)
        .to_writer_binary(&mut operation)
        .unwrap();
    PreparedAttempt {
        kind: AttemptKind::Insert,
        content,
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: anchor,
        operation,
        input_receipt_sha256: [7; 32],
    }
}
fn completion(content: &Content, anchor: u32) -> CompletionEvidence {
    CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: anchor,
        receipt_sha256: [8; 32],
    }
}
fn insert(journal: &mut Journal, c: Content) {
    let id = journal.prepare(prepared(c.clone(), 12)).unwrap();
    let permit = journal
        .mark_started(
            id,
            StartEvidence {
                ready_media_anchor: 12,
                ready_receipt_sha256: [6; 32],
                signature: [1; 21],
            },
        )
        .unwrap();
    journal.complete(permit, completion(&c, 13)).unwrap();
}
fn evidence<'a>(journal: &'a Journal, identity: &'a Identity) -> Option<JournalEvidence<'a>> {
    Some(JournalEvidence { journal, identity })
}

#[test]
fn clear_and_explicit_selection_enumerate_mixed_audio_without_authorizing_execution() {
    let catalog = music_protocol::test_support::catalogue::composer_transition_catalog(
        "Composer",
        Some("Composer"),
        1,
        true,
    );
    let (_, identity) = identities();
    let index = index(&catalog, &identity, true);
    let clear = build(&catalog, &index, &identity, None, Request::Clear).unwrap();
    assert_eq!(clear.tracks.len(), 2);
    assert_eq!(clear.proposed_removal_ids.len(), 2);
    assert_eq!(clear.entire_catalog_proposed, Some(true));
    assert!(
        clear
            .tracks
            .iter()
            .all(|t| t.unrelated_track_acknowledgement_required)
    );
    assert!(
        clear
            .tracks
            .iter()
            .any(|t| t.codec == Some("mpeg1_layer3") && t.encoded_audio_sha256.is_none())
    );
    let ids = BTreeSet::from([1000]);
    let selected = build(
        &catalog,
        &index,
        &identity,
        None,
        Request::Delete { track_ids: &ids },
    )
    .unwrap();
    assert_eq!(selected.proposed_removal_ids, vec!["00000000000003e8"]);
    assert_eq!(selected.preserved_ids, vec!["ffffffffffffffff"]);
    assert_eq!(selected.tracks[1].wire_id, "-1");
    assert!(
        build(
            &catalog,
            &index,
            &identity,
            None,
            Request::Delete {
                track_ids: &BTreeSet::from([999])
            }
        )
        .is_err()
    );
    assert!(
        build(
            &catalog,
            &index,
            &identity,
            None,
            Request::Delete {
                track_ids: &BTreeSet::new()
            }
        )
        .is_err()
    );
}

#[test]
fn prune_requires_exact_owned_binding_and_preserves_different_scope_and_present_audio() {
    let temp = Temp::new();
    let mut source = sources(&temp);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut journal = Journal::create(&temp.0.join("owned"), &id).unwrap();
    insert(
        &mut journal,
        content(&index, Some(collection_scope_sha256(&source).unwrap())),
    );
    let before = fs::read(temp.0.join("owned/journal.sqlite")).unwrap();
    let plan = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &id),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert_eq!(plan.proposed_removal_ids, vec!["ffffffffffffffff"]);
    assert!(!plan.has_blockers());
    assert!(!plan.tracks[0].unrelated_track_acknowledgement_required);
    let original = source.options.recursive;
    source.options.recursive = !original;
    let changed = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &id),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert!(changed.proposed_removal_ids.is_empty());
    assert_eq!(
        changed.tracks[0].reasons,
        vec!["different_or_unknown_source_scope"]
    );
    source.options.recursive = original;
    source.tracks[0].fingerprint = index.tracks().values().next().unwrap().fingerprint.clone();
    source.selection_stats.selected_bytes = source.tracks[0].fingerprint.file_bytes;
    let present = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &id),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert!(present.proposed_removal_ids.is_empty());
    assert_eq!(present.tracks[0].matching_source_paths, vec!["source.m4a"]);
    assert_eq!(
        fs::read(temp.0.join("owned/journal.sqlite")).unwrap(),
        before
    );
}

#[test]
fn scope_digest_keeps_its_encoding_and_filtered_empty_failed_scans_do_not_prune() {
    let temp = Temp::new();
    let mut source = sources(&temp);
    let mut blockers = Vec::new();
    // Bindings store this digest; a new encoding needs a pull to rebind them.
    let scope = serde_json::json!({"root":source.root.to_str().unwrap(),"recursive":source.options.recursive,"include":source.options.include,"exclude":source.options.exclude});
    let expected: [u8; 32] = Sha256::digest(serde_json::to_vec(&scope).unwrap()).into();
    assert_eq!(collection_scope_sha256(&source).unwrap(), expected);
    source_scope(&source, &mut blockers).unwrap();
    assert!(blockers.is_empty());
    source.options.include.insert("source.m4a".into());
    source_scope(&source, &mut blockers).unwrap();
    assert!(
        blockers
            .iter()
            .any(|b| b == "filtered_scope_cannot_authorize_prune")
    );
    source.options.include.clear();
    blockers.clear();
    // A scope without recursion prunes the tracks sent with it.
    source.options.recursive = false;
    source_scope(&source, &mut blockers).unwrap();
    assert!(blockers.is_empty());
    source.options.recursive = true;
    source.failures.push(InspectionFailure {
        relative_path: "failed.m4a".into(),
        message: "unreadable".into(),
    });
    source_scope(&source, &mut blockers).unwrap();
    assert!(
        blockers
            .iter()
            .any(|b| b == "source_inspection_incomplete_or_ambiguous")
    );
    source.failures.clear();
    source.tracks.clear();
    blockers.clear();
    source_scope(&source, &mut blockers).unwrap();
    assert!(
        blockers
            .iter()
            .any(|b| b == "empty_source_cannot_authorize_prune")
    );
    assert!(
        blockers
            .iter()
            .any(|b| b == "source_selection_coverage_mismatch")
    );
}

#[test]
fn outstanding_attempts_and_readback_drift_block_without_mutating_journal() {
    let temp = Temp::new();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut journal = Journal::create(&temp.0.join("journal"), &id).unwrap();
    let attempt = journal
        .prepare(prepared(content(&index, None), 13))
        .unwrap();
    for started in [false, true] {
        if started {
            let _ = journal
                .mark_started(
                    attempt,
                    StartEvidence {
                        ready_media_anchor: 13,
                        ready_receipt_sha256: [6; 32],
                        signature: [1; 21],
                    },
                )
                .unwrap();
        }
        let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
        let plan = build(
            &catalog,
            &index,
            &identity,
            evidence(&journal, &id),
            Request::Clear,
        )
        .unwrap();
        assert!(plan.has_blockers());
        assert!(plan.proposed_removal_ids.is_empty());
        assert_eq!(plan.entire_catalog_proposed, Some(false));
        assert_eq!(
            fs::read(temp.0.join("journal/journal.sqlite")).unwrap(),
            before
        );
    }
    let mut changed = Journal::create(&temp.0.join("changed"), &id).unwrap();
    let mut c = content(&index, None);
    c.source.full_file_sha256 = [99; 32];
    insert(&mut changed, c);
    let plan = build(
        &catalog,
        &index,
        &identity,
        evidence(&changed, &id),
        Request::Clear,
    )
    .unwrap();
    assert!(
        plan.blockers
            .iter()
            .any(|b| b.starts_with("journal_binding_drift:"))
    );
    assert!(plan.proposed_removal_ids.is_empty());
}

#[test]
fn a_file_an_interrupted_removal_took_is_no_drift_and_its_row_may_go() {
    let temp = Temp::new();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let c = content(&index(&catalog, &identity, false), None);
    let mut journal = Journal::create(&temp.0.join("journal"), &id).unwrap();
    insert(&mut journal, c.clone());
    // The phone deleted the bound track's file but kept its row.
    let gone = format!("{:016X}", c.track_pid);
    let size = catalog
        .tracks()
        .iter()
        .find(|t| t.sync_id == c.track_pid)
        .unwrap()
        .metadata
        .file_size;
    let mut value = receipt(&catalog, &identity, false);
    let records = value["records"].as_array_mut().unwrap();
    records.retain(|r| r["sync_id"] != json!(gone));
    let kept = records.len();
    value["track_count"] = json!(kept);
    value["alac_track_count"] = json!(kept);
    value["total_bytes"] = json!(value["total_bytes"].as_u64().unwrap() - size);
    value["gone_records"] = json!([gone]);
    let path = temp.0.join("index.json");
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(&path, &bytes).unwrap();
    let sha = Sha256::digest(&bytes).into();
    assert!(DeviceAudioIndex::load_complete(&path, sha, &catalog, &identity).is_err());
    let absent = Absent {
        gone: true,
        ..Absent::NONE
    };
    let partial =
        DeviceAudioIndex::load_complete_allowing(&path, sha, &catalog, &identity, absent).unwrap();
    assert_eq!(partial.gone(), &BTreeSet::from([c.track_pid]));
    let plan = build(
        &catalog,
        &partial,
        &identity,
        evidence(&journal, &id),
        Request::Delete {
            track_ids: &BTreeSet::from([c.track_pid]),
        },
    )
    .unwrap();
    assert!(!plan.has_blockers(), "{:?}", plan.blockers);
    assert!(plan.complete_audio_index_verified);
    let id = format!("{:016x}", c.track_pid);
    assert_eq!(plan.proposed_removal_ids, vec![id.clone()]);
    let row = plan.tracks.iter().find(|r| r.track_id == id).unwrap();
    assert!(row.reasons.contains(&"audio_file_already_gone"));
}

#[test]
fn pending_attempts_produce_blocked_previews_without_journal_changes() {
    use crate::engine::state::journal::test_support;
    let temp = Temp::new();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let directory = temp.0.join("journal");
    let mut journal = Journal::create(&directory, &id).unwrap();
    let c = content(&index, None);
    insert(&mut journal, c.clone());
    let attempt = test_support::prepare_insert(&mut journal, test_support::alac_content(9));
    for started in [false, true] {
        if started {
            journal
                .mark_started(
                    attempt,
                    StartEvidence {
                        ready_media_anchor: 13,
                        ready_receipt_sha256: [6; 32],
                        signature: [1; 21],
                    },
                )
                .unwrap();
        }
        let before = fs::read(directory.join("journal.sqlite")).unwrap();
        let plan = build(
            &catalog,
            &index,
            &identity,
            evidence(&journal, &id),
            Request::Clear,
        )
        .unwrap();
        assert_eq!(
            plan.blockers,
            [if started {
                "outstanding_started_requires_reconciliation"
            } else {
                "outstanding_prepared_requires_resolution"
            }]
        );
        assert!(plan.proposed_removal_ids.is_empty());
        assert_eq!(plan.entire_catalog_proposed, Some(false));
        assert!(plan.tracks.iter().all(|t| t.decision == Decision::Blocked));
        assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
    }
}

#[test]
fn namespace_or_snapshot_mismatch_refuses_before_rows_and_partial_receipt_never_becomes_complete() {
    let temp = Temp::new();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (_, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut wrong = identity.clone();
    wrong.device_identifier_sha256 = [33; 32];
    assert!(build(&catalog, &index, &wrong, None, Request::Clear).is_err());
    let other = music_protocol::test_support::catalogue::collection_catalog(14);
    assert!(build(&other, &index, &identity, None, Request::Clear).is_err());
    let mut incomplete = receipt(&catalog, &identity, false);
    incomplete["complete"] = json!(false);
    let bytes = serde_json::to_vec(&incomplete).unwrap();
    let path = temp.0.join("partial.json");
    fs::write(&path, &bytes).unwrap();
    assert!(
        DeviceAudioIndex::load_complete(&path, Sha256::digest(bytes).into(), &catalog, &identity)
            .is_err()
    );
}

#[test]
fn unavailable_clear_row_is_explicitly_blocked_and_never_an_entire_catalogue_claim() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (_, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut track = catalog.tracks()[0].clone();
    track.availability.is_placeholder = true;
    track.availability.is_supported_type = false;
    let row = decide_track(
        &track,
        &index,
        None,
        None,
        &Request::Clear,
        None,
        None,
        false,
    );
    assert_eq!(row.decision, Decision::Blocked);
    let plan = finish(
        &catalog,
        &identity,
        "clear",
        None,
        false,
        vec!["audio_index_incomplete_or_unsupported_catalog".into()],
        vec![row],
    )
    .unwrap();
    assert_eq!(plan.entire_catalog_proposed, Some(false));
    assert!(plan.proposed_removal_ids.is_empty());
    assert_eq!(plan.preserved_ids, vec!["ffffffffffffffff"]);
    assert!(plan.tracks[0].reasons.contains(&"track_unavailable"));
    assert!(plan.tracks[0].reasons.contains(&"track_type_unsupported"));
}

#[test]
fn plausible_reencoded_audio_blocks_prune_without_claiming_metadata_identity() {
    let temp = Temp::new();
    let mut source = sources(&temp);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut journal = Journal::create(&temp.0.join("journal"), &id).unwrap();
    insert(
        &mut journal,
        content(&index, Some(collection_scope_sha256(&source).unwrap())),
    );
    let target = &catalog.tracks()[0];
    source.tracks[0].metadata.title = vec![target.metadata.title.to_ascii_uppercase()];
    source.tracks[0].metadata.artist = vec![target.metadata.artist.clone()];
    source.tracks[0].fingerprint.audio.duration_ticks =
        u64::from(target.metadata.total_time_ms) * 44100 / 1000;
    let plan = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &id),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert!(plan.has_blockers());
    assert!(plan.proposed_removal_ids.is_empty());
    assert!(
        plan.tracks[0]
            .reasons
            .contains(&"possible_reencoded_source_requires_comparison")
    );
}

#[test]
fn prune_ignores_retired_ownership_and_recognizes_only_verified_readd_lifetime() {
    use crate::engine::state::journal::test_support;
    let temp = Temp::new();
    let source = sources(&temp);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (scope, identity) = identities();
    let index = index(&catalog, &identity, false);
    let current = content(&index, Some(collection_scope_sha256(&source).unwrap()));
    let mut old = current.clone();
    old.track_pid -= 1;
    old.path = "iTunes_Control/Music/F39/OLDX.m4a".into();
    let mut journal = Journal::create(&temp.0.join("journal"), &scope).unwrap();
    test_support::insert_verified(&mut journal, old.clone());
    test_support::retire(&mut journal, &old);
    let p = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &scope),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert!(p.proposed_removal_ids.is_empty());
    assert!(!p.has_blockers());
    assert_eq!(p.tracks[0].journal_origin, "unowned");
    assert!(!p.tracks[0].journal_binding_matches_audio);
    assert!(p.tracks[0].reasons.contains(&"unowned_track_preserved"));
    test_support::insert_verified(&mut journal, current);
    let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    let p = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &scope),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert_eq!(p.proposed_removal_ids, vec!["ffffffffffffffff"]);
    assert!(!p.tracks[0].unrelated_track_acknowledgement_required);
    assert_eq!(
        before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}

#[test]
fn mixed_mpeg_source_coverage_and_possible_transcode_do_not_enable_prune() {
    let temp = Temp::new();
    let source = sources(&temp);
    let root = source.root.clone();
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let mut journal = Journal::create(&temp.0.join("journal"), &id).unwrap();
    insert(
        &mut journal,
        content(&index, Some(collection_scope_sha256(&source).unwrap())),
    );
    let journal_before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
    fs::remove_file(root.join("source.m4a")).unwrap();
    let target = &catalog.tracks()[0];
    // A possible transcode must still have a valid inspected frame identity.
    // Build real coded timing and tags instead of altering the parsed report.
    let frame_count =
        usize::try_from((u64::from(target.metadata.total_time_ms) * 44_100).div_ceil(1_152_000))
            .unwrap();
    assert!((2..=10_000).contains(&frame_count));
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    fs::write(
        root.join("source.mp3"),
        mpeg_prune_tagged(
            &target.metadata.title.to_ascii_uppercase(),
            &target.metadata.artist,
            &target.metadata.album,
            &frame.repeat(frame_count),
        ),
    )
    .unwrap();
    let source = source_collection::inspect(
        &source_collection::select(&root, &SelectionOptions::default()).unwrap(),
    )
    .unwrap();
    let mut blockers = Vec::new();
    let scope = source_scope(&source, &mut blockers).unwrap();
    assert!(blockers.is_empty());
    assert_eq!(scope.sources.len(), 1);
    assert_eq!(scope.sources[0].codec, Some("mpeg1-layer3"));
    let m = &source.mp3_tracks[0].source;
    m.transferred_identity().unwrap();
    assert_eq!(m.fingerprint.audio.frame_count as usize, frame_count);
    assert_eq!(
        m.metadata.title,
        [target.metadata.title.to_ascii_uppercase()]
    );
    assert_eq!(m.metadata.artist, [target.metadata.artist.clone()]);
    let coded_ms =
        m.fingerprint.audio.coded_samples * 1000 / u64::from(m.fingerprint.audio.sample_rate);
    assert!(coded_ms.abs_diff(u64::from(target.metadata.total_time_ms)) <= 1000);
    let plan = build(
        &catalog,
        &index,
        &identity,
        evidence(&journal, &id),
        Request::Prune { sources: &source },
    )
    .unwrap();
    assert!(plan.blockers.is_empty());
    assert!(plan.has_blockers());
    assert!(plan.tracks[0].requested);
    assert_eq!(plan.tracks[0].decision, Decision::Blocked);
    assert!(plan.tracks[0].matching_source_paths.is_empty());
    assert!(plan.proposed_removal_ids.is_empty());
    assert!(
        plan.tracks[0]
            .reasons
            .contains(&"possible_reencoded_source_requires_comparison")
    );
    assert_eq!(
        journal_before,
        fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
    );
}

// A verified MPEG binding matches only its own track, path, file, codec and
// layout, and delete, clear and prune select it.

fn typed_mpeg_binding(index: &DeviceAudioIndex) -> Binding {
    use music_protocol::media::fingerprint::{EncodedMpeg1Layer3, TransferredSource};
    let row = index.other_audio().values().next().unwrap();
    let crate::engine::state::audio_index::OtherCodec::Mpeg1Layer3 {
        sample_rate,
        channels,
        frame_count,
        audio_offset,
        audio_bytes,
        trailing_tag_bytes,
    } = row.codec
    else {
        panic!("fixture is MPEG-1");
    };
    Binding {
        content: Content {
            source: TransferredSource::mpeg(
                row.file_bytes,
                row.full_file_sha256,
                EncodedMpeg1Layer3 {
                    sample_rate,
                    channels,
                    frame_count,
                    audio_offset,
                    audio_bytes,
                    trailing_bytes: u64::from(trailing_tag_bytes),
                    frame_bytes_sha256: [31; 32],
                },
            )
            .unwrap(),
            track_pid: row.sync_id,
            path: row.afc_path.clone(),
            artwork: None,
            collection: None,
        },
        receipt_kind: crate::engine::state::journal::BindingReceiptKind::InsertVerification,
        receipt_sha256: [32; 32],
        media_anchor: 13,
    }
}

#[test]
fn typed_mpeg_existing_binding_requires_full_file_path_codec_and_complete_layout() {
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET location='TEST.mp3';",
    );
    let (_, identity) = identities();
    let index = index(&catalog, &identity, true);
    let mut binding = typed_mpeg_binding(&index);
    let good = binding.content.clone();
    assert!(binding_matches(&binding, &index));
    for change in 0..4 {
        binding.content = good.clone();
        match change {
            0 => binding.content.track_pid ^= 1,
            1 => binding.content.path = "iTunes_Control/Music/F39/NOPE.mp3".into(),
            2 => binding.content.source.full_file_sha256 = [99; 32],
            _ => binding.content.source.file_bytes += 1,
        }
        assert!(
            !binding_matches(&binding, &index),
            "identity change {change}"
        );
    }
    for change in 0..6 {
        let mut m = good.source.audio.mpeg().unwrap().clone();
        match change {
            0 => m.sample_rate = 48000,
            1 => m.channels = 1,
            2 => m.frame_count += 1,
            3 => {
                m.audio_offset += 1;
                m.audio_bytes -= 1;
            }
            4 => {
                m.audio_bytes -= 128;
                m.trailing_bytes = 128;
            }
            _ => {
                m.audio_offset = 128;
                m.audio_bytes -= 128;
            }
        }
        binding.content = good.clone();
        binding.content.source = music_protocol::media::fingerprint::TransferredSource::mpeg(
            good.source.file_bytes,
            good.source.full_file_sha256,
            m,
        )
        .unwrap();
        assert!(!binding_matches(&binding, &index), "layout change {change}");
    }
    binding.content = good.clone();
    binding.content.source = fingerprint(1, good.source.file_bytes).into();
    assert!(
        !binding_matches(&binding, &index),
        "ALAC Content cannot match MPEG index"
    );
    let alac_index = super::tests::index(&catalog, &identity, false);
    binding.content = good;
    assert!(
        !binding_matches(&binding, &alac_index),
        "MPEG Content cannot match ALAC index"
    );
}

#[test]
fn owned_mpeg_target_is_selected_by_delete_clear_and_prune() {
    let temp = Temp::new();
    let source = sources(&temp);
    let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        13,
        "UPDATE item_extra SET location='TEST.mp3';",
    );
    let (id, identity) = identities();
    let index = index(&catalog, &identity, true);
    let mut binding = typed_mpeg_binding(&index);
    binding.content.collection = Some(collection_scope_sha256(&source).unwrap());
    let path = temp.0.join("journal");
    drop(Journal::create(&path, &id).unwrap());
    let mut journal = Journal::open(&path, &id).unwrap();
    crate::engine::state::journal::test_support::mpeg_verified_readback_fixture(
        &mut journal,
        binding.content.clone(),
        12,
    );
    let bytes = fs::read(path.join("journal.sqlite")).unwrap();
    for request in [
        Request::Delete {
            track_ids: &BTreeSet::from([binding.content.track_pid]),
        },
        Request::Clear,
        Request::Prune { sources: &source },
    ] {
        let plan = build(
            &catalog,
            &index,
            &identity,
            evidence(&journal, &id),
            request,
        )
        .unwrap();
        assert!(plan.blockers.is_empty());
        assert!(!plan.has_blockers(), "{:?}", plan.tracks[0].reasons);
        assert_eq!(
            plan.proposed_removal_ids,
            [format!("{:016x}", binding.content.track_pid)]
        );
        assert!(plan.tracks[0].journal_binding_matches_audio);
    }
    assert_eq!(bytes, fs::read(path.join("journal.sqlite")).unwrap());
}

struct MpegPruneFixture {
    sources: SourceCollection,
    catalog: Catalog,
    identity: IndexIdentity,
    id: Identity,
    index: DeviceAudioIndex,
    journal: Journal,
    _temp: Temp,
}
fn mpeg_prune_audio() -> Vec<u8> {
    let mut frame = vec![0; 417];
    frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
    [frame.clone(), frame].concat()
}
fn mpeg_prune_tagged(title: &str, artist: &str, album: &str, audio: &[u8]) -> Vec<u8> {
    let safe = |n: usize| -> [u8; 4] {
        [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ]
    };
    let mut body = Vec::new();
    for (name, text) in [("TIT2", title), ("TPE1", artist), ("TALB", album)] {
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(&safe(text.len() + 1));
        body.extend_from_slice(&[0, 0, 3]);
        body.extend_from_slice(text.as_bytes());
    }
    [b"ID3\x04\0\0".as_slice(), &safe(body.len()), &body, audio].concat()
}
impl MpegPruneFixture {
    fn new() -> Self {
        Self::from_bytes(&mpeg_prune_audio())
    }
    fn from_bytes(bytes: &[u8]) -> Self {
        let temp = Temp::new();
        let root = temp.0.join("source");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("owned.mp3"), bytes).unwrap();
        let sources = source_collection::inspect(
            &source_collection::select(&root, &SelectionOptions::default()).unwrap(),
        )
        .unwrap();
        assert_eq!(sources.mp3_tracks.len(), 1);
        let source = sources.mp3_tracks[0].source.transferred_identity().unwrap();
        let catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            13,
            &format!(
                "UPDATE item_extra SET location='TEST.mp3',file_size={},total_time_ms=52;",
                source.file_bytes
            ),
        );
        let (id, identity) = identities();
        let mut value = receipt(&catalog, &identity, true);
        let record = &mut value["other_records"][0];
        let m = source.audio.mpeg().unwrap();
        record["full_file_sha256"] = json!(hex(&source.full_file_sha256));
        record["codec"] = json!({"kind":"mpeg1-layer3","sample_rate":m.sample_rate,"channels":m.channels,"frame_count":m.frame_count,"audio_offset":m.audio_offset,"audio_bytes":m.audio_bytes,"trailing_tag_bytes":m.trailing_bytes});
        let bytes = serde_json::to_vec(&value).unwrap();
        let index_path = temp.0.join("index.json");
        fs::write(&index_path, &bytes).unwrap();
        let index = DeviceAudioIndex::load_complete(
            &index_path,
            Sha256::digest(bytes).into(),
            &catalog,
            &identity,
        )
        .unwrap();
        let content = Content {
            source,
            track_pid: catalog.tracks()[0].sync_id,
            path: catalog.tracks()[0].afc_path.clone().unwrap(),
            artwork: None,
            collection: Some(collection_scope_sha256(&sources).unwrap()),
        };
        let path = temp.0.join("journal");
        drop(Journal::create(&path, &id).unwrap());
        let mut journal = Journal::open(&path, &id).unwrap();
        crate::engine::state::journal::test_support::mpeg_verified_readback_fixture(
            &mut journal,
            content,
            12,
        );
        drop(journal);
        let journal = Journal::open_read_only(&path, &id).unwrap();
        assert!(index.ensure_matches(&catalog, &identity).is_ok());
        let binding = journal.active_verified_bindings().unwrap().remove(0);
        assert_eq!(
            binding.receipt_kind,
            crate::engine::state::journal::BindingReceiptKind::InsertVerification
        );
        Self {
            sources,
            catalog,
            identity,
            id,
            index,
            journal,
            _temp: temp,
        }
    }
    fn inspect(&self) -> SourceCollection {
        source_collection::inspect(
            &source_collection::select(&self.sources.root, &SelectionOptions::default()).unwrap(),
        )
        .unwrap()
    }
    fn plan(&self, sources: &SourceCollection) -> Plan {
        let before = self.journal.recheck_read_only_source().unwrap();
        let plan = build(
            &self.catalog,
            &self.index,
            &self.identity,
            evidence(&self.journal, &self.id),
            Request::Prune { sources },
        )
        .unwrap();
        assert_eq!(self.journal.recheck_read_only_source().unwrap(), before);
        plan
    }
}

#[test]
fn prune_preserves_present_owned_mpeg_and_records_exact_source_path() {
    let f = MpegPruneFixture::new();
    let plan = f.plan(&f.sources);
    assert!(plan.complete_audio_index_verified);
    assert!(plan.blockers.is_empty());
    assert!(!plan.has_blockers());
    assert!(plan.proposed_removal_ids.is_empty());
    assert_eq!(plan.preserved_ids, ["ffffffffffffffff"]);
    let row = &plan.tracks[0];
    assert!(row.journal_binding_matches_audio);
    assert!(!row.requested);
    assert_eq!(row.decision, Decision::Preserved);
    assert_eq!(row.reasons, ["encoded_audio_present_in_source"]);
    assert_eq!(row.matching_source_paths, ["owned.mp3"]);
}

#[test]
fn prune_preserves_mpeg_same_layout_tag_edits_without_requiring_full_file_equality() {
    let audio = mpeg_prune_audio();
    let f = MpegPruneFixture::from_bytes(&mpeg_prune_tagged("Before", "Artist", "Album", &audio));
    fs::write(
        f.sources.root.join("owned.mp3"),
        mpeg_prune_tagged("Edited", "Artist", "Album", &audio),
    )
    .unwrap();
    let sources = f.inspect();
    assert_ne!(
        sources.mp3_tracks[0].source.metadata.title,
        f.sources.mp3_tracks[0].source.metadata.title
    );
    assert_ne!(
        sources.mp3_tracks[0].source.fingerprint.full_file_sha256,
        f.sources.mp3_tracks[0].source.fingerprint.full_file_sha256
    );
    let plan = f.plan(&sources);
    assert!(!plan.has_blockers());
    assert!(!plan.tracks[0].requested);
    assert_eq!(plan.tracks[0].reasons, ["encoded_audio_present_in_source"]);
    assert_eq!(plan.tracks[0].matching_source_paths, ["owned.mp3"]);
}

#[test]
fn prune_selects_an_absent_owned_mpeg() {
    let f = MpegPruneFixture::new();
    fs::remove_file(f.sources.root.join("owned.mp3")).unwrap();
    fs::copy(
        crate::test_fixtures::workspace_root().join("tests/fixtures/silence-alac.m4a"),
        f.sources.root.join("unrelated.m4a"),
    )
    .unwrap();
    let sources = f.inspect();
    assert_eq!(sources.tracks.len(), 1);
    assert!(sources.mp3_tracks.is_empty());
    let plan = f.plan(&sources);
    assert!(plan.blockers.is_empty());
    assert!(plan.tracks[0].requested);
    assert_eq!(plan.tracks[0].decision, Decision::ProposedRemoval);
    assert!(plan.tracks[0].reasons.is_empty());
    assert!(plan.tracks[0].matching_source_paths.is_empty());
    assert_eq!(plan.proposed_removal_ids, ["ffffffffffffffff"]);
}

#[test]
fn prune_mpeg_layout_variants_preserve_checked_encoded_audio() {
    for (leading, trailing) in [(true, false), (false, true), (true, true)] {
        let f = MpegPruneFixture::new();
        let mut bytes = mpeg_prune_audio();
        if leading {
            bytes = mpeg_prune_tagged("Tagged", "Artist", "Album", &bytes);
        }
        if trailing {
            let mut tag = vec![0; 128];
            tag[..3].copy_from_slice(b"TAG");
            bytes.extend(tag);
        }
        fs::write(f.sources.root.join("owned.mp3"), bytes).unwrap();
        let sources = f.inspect();
        assert_eq!(
            sources.mp3_tracks[0].source.fingerprint.audio,
            f.sources.mp3_tracks[0].source.fingerprint.audio
        );
        assert!(
            sources.mp3_tracks[0]
                .source
                .transferred_identity()
                .unwrap()
                .audio
                != f.sources.mp3_tracks[0]
                    .source
                    .transferred_identity()
                    .unwrap()
                    .audio
        );
        let plan = f.plan(&sources);
        assert!(plan.complete_audio_index_verified);
        assert!(plan.blockers.is_empty());
        assert!(!plan.has_blockers());
        assert!(plan.proposed_removal_ids.is_empty());
        assert_eq!(plan.preserved_ids, ["ffffffffffffffff"]);
        let row = &plan.tracks[0];
        assert!(row.journal_binding_matches_audio);
        assert!(!row.requested);
        assert_eq!(row.decision, Decision::Preserved);
        assert_eq!(row.matching_source_paths, ["owned.mp3"]);
        assert_eq!(row.reasons, ["encoded_audio_present_in_source"]);
    }
}

#[test]
fn prune_mpeg_inconsistent_descriptors_block_source_absence() {
    let f = MpegPruneFixture::new();
    for change in 0..8 {
        // Each report starts from a normally parsed file. These mutations
        // isolate a corrupt/incomplete descriptor from filesystem inspection.
        let mut sources = f.inspect();
        let source = &mut sources.mp3_tracks[0].source;
        match change {
            0 => source.fingerprint.audio.frame_bytes_sha256 = hex(&[42; 32]),
            1 => source.fingerprint.audio.channels = 1,
            2 => source.fingerprint.audio.sample_rate = 48000,
            3 => source.fingerprint.audio.coded_samples += 1,
            4 => source.fingerprint.audio.format = "unknown-format",
            5 => source.fingerprint.full_file_sha256 = "invalid-digest".into(),
            6 => source.fingerprint.audio.sample_rate = 0,
            _ => source.audio_offset += 1,
        }
        assert_eq!(
            source.fingerprint.audio.sha256,
            f.sources.mp3_tracks[0].source.fingerprint.audio.sha256
        );
        assert!(source.transferred_identity().is_err());
        let plan = f.plan(&sources);
        assert_eq!(plan.blockers, ["source_mpeg_identity_invalid"]);
        assert!(plan.has_blockers());
        assert!(!plan.tracks[0].requested);
        assert_eq!(plan.tracks[0].decision, Decision::Preserved);
        assert!(plan.tracks[0].matching_source_paths.is_empty());
        assert_eq!(plan.tracks[0].reasons, ["source_mpeg_identity_invalid"]);
        assert!(plan.proposed_removal_ids.is_empty());
        assert_eq!(plan.preserved_ids, ["ffffffffffffffff"]);
    }
}

#[test]
fn prune_mpeg_reencoded_audio_with_matching_tags_remains_ambiguous() {
    let f = MpegPruneFixture::new();
    let mut audio = mpeg_prune_audio();
    *audio.last_mut().unwrap() = 1;
    let metadata = &f.catalog.tracks()[0].metadata;
    fs::write(
        f.sources.root.join("owned.mp3"),
        mpeg_prune_tagged(&metadata.title, &metadata.artist, &metadata.album, &audio),
    )
    .unwrap();
    let sources = f.inspect();
    assert_ne!(
        sources.mp3_tracks[0].source.fingerprint.audio.sha256,
        f.sources.mp3_tracks[0].source.fingerprint.audio.sha256
    );
    let plan = f.plan(&sources);
    assert!(plan.blockers.is_empty());
    assert!(plan.has_blockers());
    assert!(plan.tracks[0].requested);
    assert_eq!(plan.tracks[0].decision, Decision::Blocked);
    assert!(plan.tracks[0].matching_source_paths.is_empty());
    assert!(
        plan.tracks[0]
            .reasons
            .contains(&"possible_reencoded_source_requires_comparison")
    );
}

#[test]
fn prune_mpeg_duplicate_files_and_hardlink_aliases_still_require_review() {
    for hardlink in [false, true] {
        let f = MpegPruneFixture::new();
        let owned = f.sources.root.join("owned.mp3");
        let alias = f.sources.root.join("alias.mp3");
        if hardlink {
            fs::hard_link(owned, alias).unwrap();
        } else {
            fs::copy(owned, alias).unwrap();
        }
        let sources = f.inspect();
        assert_eq!(sources.mp3_tracks.len(), 2);
        let plan = f.plan(&sources);
        assert!(
            plan.blockers
                .contains(&"source_inspection_incomplete_or_ambiguous".into())
        );
        assert!(plan.has_blockers());
        assert!(!plan.tracks[0].requested);
        assert_eq!(plan.tracks[0].decision, Decision::Preserved);
        assert_eq!(
            plan.tracks[0].matching_source_paths,
            ["alias.mp3", "owned.mp3"]
        );
        assert!(plan.proposed_removal_ids.is_empty());
    }
}

#[test]
fn prune_mpeg_exact_presence_does_not_clear_scan_or_scope_refusals() {
    for change in 0..5 {
        let f = MpegPruneFixture::new();
        let mut sources = f.inspect();
        let expected = match change {
            0 => {
                sources.selection_stats.selected_files += 1;
                "source_selection_coverage_mismatch"
            }
            1 => {
                fs::write(f.sources.root.join("broken.mp3"), b"not MPEG").unwrap();
                sources = f.inspect();
                "source_inspection_incomplete_or_ambiguous"
            }
            2 => {
                std::os::unix::fs::symlink("owned.mp3", f.sources.root.join("link.mp3")).unwrap();
                sources = f.inspect();
                "unsafe_or_narrowed_source_exclusions"
            }
            3 => {
                let mut options = SelectionOptions::default();
                options.include.insert("owned.mp3".into());
                sources = source_collection::inspect(
                    &source_collection::select(&f.sources.root, &options).unwrap(),
                )
                .unwrap();
                "filtered_scope_cannot_authorize_prune"
            }
            _ => {
                sources = source_collection::inspect(
                    &source_collection::select(
                        &f.sources.root.join("owned.mp3"),
                        &SelectionOptions::default(),
                    )
                    .unwrap(),
                )
                .unwrap();
                "prune_requires_full_directory_scope"
            }
        };
        let plan = f.plan(&sources);
        assert!(
            plan.blockers.contains(&expected.into()),
            "scan change {change}: {:?}",
            plan.blockers
        );
        assert!(plan.has_blockers());
        assert!(!plan.tracks[0].requested);
        assert_eq!(plan.tracks[0].decision, Decision::Preserved);
        assert!(plan.proposed_removal_ids.is_empty());
        if change >= 3 {
            assert_eq!(
                plan.tracks[0].reasons,
                ["different_or_unknown_source_scope"]
            );
            assert!(plan.tracks[0].matching_source_paths.is_empty());
        } else {
            assert_eq!(plan.tracks[0].reasons, ["encoded_audio_present_in_source"]);
            assert_eq!(plan.tracks[0].matching_source_paths, ["owned.mp3"]);
        }
    }
}

#[test]
fn remove_missing_takes_only_owned_tracks_whose_reached_source_is_gone() {
    let temp = Temp::new();
    let mut source = sources(&temp);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let (id, identity) = identities();
    let index = index(&catalog, &identity, false);
    let track = index.tracks().values().next().unwrap().sync_id;
    let gone = source.root.join("gone.m4a");
    let mut journal = Journal::create(&temp.0.join("owned"), &id).unwrap();
    insert(&mut journal, content(&index, None));
    let before = fs::read(temp.0.join("owned/journal.sqlite")).unwrap();
    let missing = |origin: Origin| BTreeMap::from([(track, origin)]);
    let plan = |journal: Option<&Journal>,
                sources: Option<&SourceCollection>,
                origins: &BTreeMap<u64, Origin>| {
        build(
            &catalog,
            &index,
            &identity,
            journal.and_then(|journal| evidence(journal, &id)),
            Request::Missing { sources, origins },
        )
        .unwrap()
    };

    let selected = plan(
        Some(&journal),
        Some(&source),
        &missing(Origin::Missing(gone.clone())),
    );
    assert_eq!(selected.mode, "remove_missing");
    assert_eq!(selected.proposed_removal_ids, vec![format!("{track:016x}")]);
    assert!(!selected.has_blockers());
    assert!(!selected.tracks[0].unrelated_track_acknowledgement_required);
    assert_eq!(
        selected.tracks[0].missing_source_path.as_deref(),
        gone.to_str()
    );
    let json = serde_json::to_value(&selected).unwrap();
    assert_eq!(json["tracks"][0]["missing_source_path"], json!(gone));

    for (origin, reason) in [
        (Origin::Present, "source_present"),
        (Origin::Outside, "source_outside_selection"),
        (Origin::Unknown("no input".into()), "source_path_unknown"),
    ] {
        let kept = plan(Some(&journal), Some(&source), &missing(origin));
        assert!(kept.proposed_removal_ids.is_empty() && !kept.has_blockers());
        assert_eq!(kept.tracks[0].reasons, vec![reason]);
        assert!(
            serde_json::to_value(&kept).unwrap()["tracks"][0]
                .get("missing_source_path")
                .is_none()
        );
    }
    let kept = plan(Some(&journal), Some(&source), &BTreeMap::new());
    assert_eq!(kept.tracks[0].reasons, vec!["source_path_unknown"]);
    // Nothing owned: nothing to remove and nothing to refuse.
    let unjournaled = plan(None, None, &BTreeMap::new());
    assert!(unjournaled.proposed_removal_ids.is_empty() && !unjournaled.has_blockers());
    assert_eq!(
        unjournaled.tracks[0].reasons,
        vec!["unowned_track_preserved"]
    );

    // A file moved within the selection keeps its track.
    let mut moved = sources(&Temp::new());
    moved.tracks[0].fingerprint = index.tracks()[&track].fingerprint.clone();
    let kept = plan(
        Some(&journal),
        Some(&moved),
        &missing(Origin::Missing(gone.clone())),
    );
    assert!(kept.proposed_removal_ids.is_empty() && !kept.has_blockers());
    assert_eq!(
        kept.tracks[0].reasons,
        vec!["encoded_audio_present_in_source"]
    );
    assert_eq!(kept.tracks[0].matching_source_paths, vec!["source.m4a"]);

    // A gone source is judged only against a complete, nonempty inspection.
    let blocked = plan(
        Some(&journal),
        None,
        &missing(Origin::Missing(gone.clone())),
    );
    assert_eq!(blocked.blockers, vec!["source_inspection_absent"]);
    assert_eq!(blocked.tracks[0].decision, Decision::Blocked);
    assert!(!blocked.held_by_unreadable_sources());
    source.failures.push(InspectionFailure {
        relative_path: "failed.m4a".into(),
        message: "unreadable".into(),
    });
    let blocked = plan(
        Some(&journal),
        Some(&source),
        &missing(Origin::Missing(gone.clone())),
    );
    assert_eq!(blocked.blockers, vec!["source_inspection_incomplete"]);
    assert!(blocked.proposed_removal_ids.is_empty());
    // A file sync skips because it could not be read may be the gone source,
    // moved: the removal waits for it, while the rest of the sync goes on.
    assert!(blocked.held_by_unreadable_sources());
    assert_eq!(blocked.tracks[0].decision, Decision::Blocked);
    // Even when its own path is reached and gone, a skipped file that reads
    // keeps its track by its audio.
    moved.failures.push(InspectionFailure {
        relative_path: "failed.m4a".into(),
        message: "unreadable".into(),
    });
    let held = plan(
        Some(&journal),
        Some(&moved),
        &missing(Origin::Missing(gone.clone())),
    );
    assert!(held.proposed_removal_ids.is_empty());
    assert_eq!(held.tracks[0].decision, Decision::Preserved);
    moved.failures.clear();
    source.failures.clear();
    source.tracks.clear();
    source.selection_stats.selected_files = 0;
    let blocked = plan(
        Some(&journal),
        Some(&source),
        &missing(Origin::Missing(gone.clone())),
    );
    assert_eq!(
        blocked.blockers,
        vec!["empty_source_cannot_authorize_removal"]
    );
    assert!(!blocked.held_by_unreadable_sources());
    // An empty selection with nothing gone is no removal request at all.
    assert!(!plan(Some(&journal), Some(&source), &missing(Origin::Present)).has_blockers());
    assert_eq!(
        fs::read(temp.0.join("owned/journal.sqlite")).unwrap(),
        before
    );
}
