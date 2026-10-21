use super::*;
use crate::engine::state::journal::{Artwork, CompletionEvidence};
use music_protocol::media::fingerprint;
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "native-artwork-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn source(&self, with_art: bool) -> PathBuf {
        let mut bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/silence-alac.m4a"
        ))
        .to_vec();
        if with_art {
            let jpeg = [0xff, 0xd8, 0xff, 0xd9];
            let mut data = vec![0, 0, 0, 13, 0, 0, 0, 0];
            data.extend(jpeg);
            let cover = atom(b"covr", &atom(b"data", &data));
            for at in [80, 665, 673, 718] {
                let n = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
                bytes[at..at + 4].copy_from_slice(&(n + cover.len() as u32).to_be_bytes());
            }
            bytes.extend(cover);
        }
        let padding = 12345 - bytes.len() - 8;
        bytes.extend(atom(b"free", &vec![0; padding]));
        let path = self.0.join(if with_art {
            "desired.m4a"
        } else {
            "transferred.m4a"
        });
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(body);
    out
}
fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn saved_art(next: u32) -> Vec<u8> {
    let mut bytes = vec![0; 132 + 96 + 92];
    bytes[..4].copy_from_slice(b"mhfd");
    put(&mut bytes, 4, 132);
    put(&mut bytes, 8, 320);
    put(&mut bytes, 16, 6);
    put(&mut bytes, 20, 1);
    put(&mut bytes, 28, next);
    bytes[132..136].copy_from_slice(b"mhsd");
    put(&mut bytes, 136, 96);
    put(&mut bytes, 140, 188);
    put(&mut bytes, 144, 1);
    bytes[228..232].copy_from_slice(b"mhli");
    put(&mut bytes, 232, 92);
    bytes
}
fn cdb(catalog: &Catalog) -> Vec<u8> {
    let mut bytes = vec![0; 0xf4 + 0x60 + 0x30];
    bytes[..4].copy_from_slice(b"mhbd");
    put(&mut bytes, 4, 0xf4);
    put(&mut bytes, 8, 388);
    put(&mut bytes, 12, 1);
    put(&mut bytes, 16, 117);
    put(&mut bytes, 20, 1);
    bytes[0x48..0x50].copy_from_slice(&catalog.database_pid().to_le_bytes());
    bytes[0xf4..0xf8].copy_from_slice(b"mhsd");
    put(&mut bytes, 0xf8, 0x60);
    put(&mut bytes, 0xfc, 0x90);
    put(&mut bytes, 0x100, 11);
    put(&mut bytes, 0x154, 1);
    put(&mut bytes, 0x15c, 1);
    bytes
}
fn prefs() -> Vec<u8> {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&1u64.to_le_bytes());
    raw[20..28].copy_from_slice(&2u64.to_le_bytes());
    raw
}
fn state(catalog: &Catalog) -> FreshArtworkState<'_> {
    FreshArtworkState::new(
        "synthetic",
        catalog,
        &prefs(),
        &prefs(),
        &saved_art(677),
        &cdb(catalog),
        [7; 32],
    )
    .unwrap()
}
fn journal(temp: &Temp, source: &Path) -> Journal {
    let mut journal = Journal::create(
        &temp.0.join("journal"),
        &Identity::new(
            "synthetic",
            &music_protocol::catalogue::identity::parse(&prefs()).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let content = Content {
        source: (fingerprint::read(source).unwrap()).into(),
        track_pid: u64::MAX,
        path: "iTunes_Control/Music/F39/TEST.m4a".into(),
        artwork: None,
        collection: Some([8; 32]),
    };
    let evidence = CompletionEvidence {
        track_pid: content.track_pid,
        path: content.path.clone(),
        audio_bytes: content.source.file_bytes,
        audio_file_sha256: content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 13,
        receipt_sha256: [9; 32],
    };
    crate::engine::state::journal::test_support::insert_verified_with(
        &mut journal,
        content,
        evidence,
    );
    journal
}
fn prepare(journal: &mut Journal, catalog: &Catalog, source: &Path) -> AttemptId {
    let cover = artwork::read(source).unwrap().cover.unwrap();
    let mut content = journal
        .active_verified_bindings()
        .unwrap()
        .remove(0)
        .content;
    content.artwork = Some(Artwork {
        cache_id: allocate_cache_id(catalog, &saved_art(677), journal).unwrap(),
        bytes: cover.bytes.len() as u64,
        sha256: cover.sha256,
    });
    let operation = artwork_update::prepare(
        catalog,
        content.track_pid,
        content.artwork.as_ref().unwrap().cache_id,
        &cover,
        SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    journal
        .prepare(PreparedAttempt {
            kind: AttemptKind::Artwork,
            content,
            new_album_pid: None,
            new_artist_pid: None,
            current_media_anchor: 13,
            operation,
            input_receipt_sha256: [7; 32],
        })
        .unwrap()
}
fn control() -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(180),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn allocator_unions_saved_sql_and_all_history_without_reusing_counter() {
    let mut saved = artwork_inventory::parse(&saved_art(677)).unwrap();
    assert_eq!(
        next_cache(&saved, &BTreeSet::from([0, 677]), &BTreeSet::new()).unwrap(),
        678
    );
    saved.images.insert(800, 42);
    assert_eq!(
        next_cache(&saved, &BTreeSet::from([900]), &BTreeSet::from([1000])).unwrap(),
        1001
    );
    assert!(next_cache(&saved, &BTreeSet::from([u32::MAX]), &BTreeSet::new()).is_err());
    saved.next_record_id = 99;
    assert!(next_cache(&saved, &BTreeSet::new(), &BTreeSet::new()).is_err());
}

#[test]
fn cancelled_reservation_is_never_reused_and_outstanding_blocks_allocation() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    assert!(allocate_cache_id(&catalog, &saved_art(677), &journal).is_err());
    journal.cancel_prepared(id).unwrap();
    assert_eq!(
        allocate_cache_id(&catalog, &saved_art(677), &journal).unwrap(),
        679
    );
}

#[test]
fn desired_cover_edit_preserves_transferred_audio_and_rebuilds_exact_operation() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let state = state(&catalog);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    let mut attempt = journal.attempt(id).unwrap();
    let expected = fingerprint::read(&desired).unwrap();
    let cover = artwork::read(&desired).unwrap().cover.unwrap();
    assert_ne!(
        expected.full_file_sha256,
        attempt.prepared.content.source.full_file_sha256
    );
    assert!(expected.audio == attempt.prepared.content.source.audio);
    validate_preparation(&journal, &attempt.prepared, &state, &expected, &cover).unwrap();
    // A retagged source is not substituted for the old permanent audio hash.
    attempt.prepared.content.source = expected.clone().into();
    assert!(validate_preparation(&journal, &attempt.prepared, &state, &expected, &cover).is_err());
    let mut attempt = journal.attempt(id).unwrap();
    let mut value = Value::from_reader(Cursor::new(&attempt.prepared.operation)).unwrap();
    value
        .as_dictionary_mut()
        .unwrap()
        .get_mut("operations")
        .unwrap()
        .as_array_mut()
        .unwrap()[0]
        .as_dictionary_mut()
        .unwrap()
        .insert("pid".into(), 42i64.into());
    attempt.prepared.operation.clear();
    value
        .to_writer_binary(&mut attempt.prepared.operation)
        .unwrap();
    assert!(validate_preparation(&journal, &attempt.prepared, &state, &expected, &cover).is_err());
}

#[test]
fn wrong_anchor_receipt_audio_cover_and_cache_fail_preparation() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let state = state(&catalog);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    let expected = fingerprint::read(&desired).unwrap();
    let cover = artwork::read(&desired).unwrap().cover.unwrap();
    for change in 0..6 {
        let mut attempt = journal.attempt(id).unwrap();
        let mut source = expected.clone();
        match change {
            0 => attempt.prepared.current_media_anchor += 1,
            1 => attempt.prepared.input_receipt_sha256[0] ^= 1,
            2 => source.audio.sha256[0] ^= 1,
            3 => attempt.prepared.content.artwork.as_mut().unwrap().sha256[0] ^= 1,
            4 => attempt.prepared.content.artwork.as_mut().unwrap().cache_id += 1,
            _ => attempt.prepared.content.path = "iTunes_Control/Music/F39/NOPE.m4a".into(),
        }
        assert!(
            validate_preparation(&journal, &attempt.prepared, &state, &source, &cover).is_err()
        );
    }
}

#[tokio::test]
async fn guards_and_discovery_failure_leave_prepared_and_never_connect_on_bad_source() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let state = state(&catalog);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    let expected = fingerprint::read(&desired).unwrap();
    let calls = AtomicUsize::new(0);
    for valid in [false, true] {
        let mut fingerprint = expected.clone();
        if !valid {
            fingerprint.full_file_sha256[0] ^= 1;
        }
        let trace = temp.0.join(if valid { "valid" } else { "invalid" });
        let result = execute(
            &mut journal,
            ArtworkUpdate {
                attempt: id,
                source: &desired,
                desired_source: &fingerprint,
                state: &state,
                host_name: "Synthetic",
                trace: &trace,
            },
            &control(),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("synthetic connector refuses discovery")
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(valid));
        assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
        if valid {
            assert_eq!(
                fs::read(trace.join("source-cover.jpg")).unwrap(),
                vec![0xff, 0xd8, 0xff, 0xd9]
            );
            assert!(!trace.join("operation.cig").exists());
        } else {
            assert!(!trace.exists());
        }
    }
}

#[test]
fn cdb_binding_and_deadline_budget_are_strict() {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut wrong = cdb(&catalog);
    wrong[0x48] ^= 1;
    assert!(
        FreshArtworkState::new(
            "synthetic",
            &catalog,
            &prefs(),
            &prefs(),
            &saved_art(677),
            &wrong,
            [7; 32]
        )
        .is_err()
    );
    let state = state(&catalog);
    assert!(
        operation_budget(
            state.common.state_read_chunks().unwrap(),
            state.artwork_bytes,
            12345,
            4
        )
        .unwrap()
            < 1024
    );
    assert!(operation_budget(u64::MAX, 0, 0, 0).is_err());
    assert!(operation_budget(4000, MAX_ASSET, MAX_AUDIO, MAX_ASSET).is_err());
    assert!(operation_budget(1, MAX_ASSET + 1, 1, 1).is_err());
}

#[test]
fn evidence_distinguishes_desired_source_from_permanent_audio() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    let content = journal.attempt(id).unwrap().prepared.content;
    let desired = fingerprint::read(&desired).unwrap();
    let evidence = ArtworkEvidence {
        permanent_audio_sha256: content.source.full_file_sha256,
        staged_cover_sha256: content.artwork.as_ref().unwrap().sha256,
        content,
        desired_source_sha256: desired.full_file_sha256,
        desired_source_bytes: desired.file_bytes,
        operation_sha256: [1; 32],
        ready_receipt_sha256: [2; 32],
        terminal_receipt_sha256: [3; 32],
        media_anchor: 14,
        receipt_sha256: [0; 32],
    };
    let value = Value::from_reader(Cursor::new(evidence_plist(&evidence).unwrap())).unwrap();
    let d = value.as_dictionary().unwrap();
    assert_ne!(d["PermanentAudioSHA256"], d["DesiredSourceSHA256"]);
    assert_eq!(
        d["Status"].as_string(),
        Some("PendingIndependentCatalogVerification")
    );
    assert_eq!(d["CacheID"].as_unsigned_integer(), Some(678));
    assert!(d.contains_key("StagedCoverSHA256"));
    assert!(!d.contains_key("CoverReadbackSHA256"));
}

#[tokio::test]
async fn interrupted_started_and_cancelled_deadline_never_open_connector() {
    let temp = Temp::new();
    let original = temp.source(false);
    let desired = temp.source(true);
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let state = state(&catalog);
    let mut journal = journal(&temp, &original);
    let id = prepare(&mut journal, &catalog, &desired);
    let expected = fingerprint::read(&desired).unwrap();
    let trace = temp.0.join("never-created");
    for expired in [false, true] {
        let mut control = control();
        if expired {
            control.deadline = Instant::now();
        } else {
            control.cancelled.store(true, Ordering::Relaxed);
        }
        let result = execute(
            &mut journal,
            ArtworkUpdate {
                attempt: id,
                source: &desired,
                desired_source: &expected,
                state: &state,
                host_name: "Synthetic",
                trace: &trace,
            },
            &control,
            || async { panic!("cancelled artwork reached connector") },
        )
        .await;
        assert!(result.is_err());
    }
    let permit = journal
        .mark_started(
            id,
            StartEvidence {
                ready_media_anchor: 13,
                ready_receipt_sha256: [1; 32],
                signature: [1; 21],
            },
        )
        .unwrap();
    drop(permit);
    drop(journal);
    let mut journal = Journal::open(
        &temp.0.join("journal"),
        &Identity::new(
            "synthetic",
            &music_protocol::catalogue::identity::parse(&prefs()).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let result = execute(
        &mut journal,
        ArtworkUpdate {
            attempt: id,
            source: &desired,
            desired_source: &expected,
            state: &state,
            host_name: "Synthetic",
            trace: &trace,
        },
        &control(),
        || async { panic!("interrupted artwork reached connector") },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        journal.attempt(id).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    assert!(!trace.exists());
}
