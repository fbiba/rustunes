use super::*;
use crate::engine::state::current::Current;
use crate::engine::state::journal::{AttemptState, Content, StartEvidence, test_support};
use music_protocol::catalogue::identity;
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);

#[tokio::test]
async fn ordinary_local_refusal_never_opens_writable_journal() {
    for prepared in [false, true] {
        let f = Fixture::new();
        let mut journal = f.journal();
        if prepared {
            test_support::prepare_insert(&mut journal, f.content());
        }
        drop(journal);
        let directory = f.root.join("journal");
        let before = fs::read(directory.join("journal.sqlite")).unwrap();
        let read_only = Journal::open_read_only(&directory, &f.identity).unwrap();
        let trace = f.root.join("refusal");
        let opens = std::cell::Cell::new(0);
        let out = run_ordinary(
            &f.options(&trace, None),
            &read_only,
            &f.identity,
            &crate::engine::test_engine(),
            |_, _| async { panic!("local recovery must not capture the phone") },
            || {
                opens.set(opens.get() + 1);
                Journal::open(&directory, &f.identity)
            },
        )
        .await
        .unwrap();
        assert_eq!(opens.get(), 0, "local recovery opened a writable journal");
        assert_eq!(
            out.status,
            if prepared {
                "uncertain"
            } else {
                "no_outstanding"
            }
        );
        assert!(!out.journal_changed && !trace.exists());
        assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
    }
}

#[tokio::test]
async fn ordinary_failed_current_capture_never_opens_writable_journal() {
    let f = Fixture::new();
    let mut journal = f.journal();
    let id = test_support::prepare_insert(&mut journal, f.content());
    drop(
        journal
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: 13,
                    ready_receipt_sha256: [8; 32],
                    signature: [1; 21],
                },
            )
            .unwrap(),
    );
    drop(journal);
    let directory = f.root.join("journal");
    let before = fs::read(directory.join("journal.sqlite")).unwrap();
    let read_only = Journal::open_read_only(&directory, &f.identity).unwrap();
    let trace = f.root.join("failed-capture");
    let opens = std::cell::Cell::new(0);
    let result = run_ordinary(
        &f.options(&trace, None),
        &read_only,
        &f.identity,
        &crate::engine::test_engine(),
        |_, _| async { anyhow::bail!("synthetic Current capture refused") },
        || {
            opens.set(opens.get() + 1);
            Journal::open(&directory, &f.identity)
        },
    )
    .await;
    assert!(format!("{:#}", result.err().unwrap()).contains("synthetic Current capture refused"));
    assert_eq!(opens.get(), 0, "failed capture opened a writable journal");
    assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
    assert!(
        matches!(read_only.outstanding().unwrap(), Some(a) if a.id == id && a.state == AttemptState::ReconciliationRequired)
    );
}

#[tokio::test]
async fn ordinary_failed_completion_checks_never_open_writable_journal() {
    let f = Fixture::new();
    let mut journal = f.journal();
    let id = test_support::prepare_insert(&mut journal, f.content());
    drop(
        journal
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: 13,
                    ready_receipt_sha256: [8; 32],
                    signature: [1; 21],
                },
            )
            .unwrap(),
    );
    drop(journal);
    let directory = f.root.join("journal");
    let before = fs::read(directory.join("journal.sqlite")).unwrap();
    let read_only = Journal::open_read_only(&directory, &f.identity).unwrap();
    let trace = f.root.join("failed-completion");
    let opens = std::cell::Cell::new(0);
    let out = run_ordinary(
        &f.options(&trace, None),
        &read_only,
        &f.identity,
        &crate::engine::test_engine(),
        |_, _| async { Ok(f.current(14)) },
        || {
            opens.set(opens.get() + 1);
            Journal::open(&directory, &f.identity)
        },
    )
    .await
    .unwrap();
    assert_eq!(out.status, "uncertain");
    assert!(out.reason.unwrap().contains("no retained step"));
    assert_eq!(opens.get(), 0);
    assert!(!out.journal_changed);
    assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
}

#[tokio::test]
async fn ordinary_capture_cancellation_remains_original_before_completion() {
    let f = Fixture::new();
    let mut journal = f.journal();
    let id = test_support::prepare_insert(&mut journal, f.content());
    drop(
        journal
            .mark_started(
                id,
                StartEvidence {
                    ready_media_anchor: 13,
                    ready_receipt_sha256: [8; 32],
                    signature: [1; 21],
                },
            )
            .unwrap(),
    );
    drop(journal);
    let directory = f.root.join("journal");
    let before = fs::read(directory.join("journal.sqlite")).unwrap();
    let read_only = Journal::open_read_only(&directory, &f.identity).unwrap();
    let trace = f.root.join("cancelled-capture");
    let result = run_ordinary(
        &f.options(&trace, None),
        &read_only,
        &f.identity,
        &crate::engine::test_engine(),
        |deadline, cancelled| {
            let current = f.current(14);
            async move {
                assert!(
                    deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(180)
                );
                cancelled.store(true, Ordering::Relaxed);
                Ok(current)
            }
        },
        || panic!("capture cancellation must never open the writer"),
    )
    .await;
    assert!(format!("{:#}", result.err().unwrap()).contains("cancelled"));
    assert!(!trace.join("reconciliation").exists());
    assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
}

struct Fixture {
    root: PathBuf,
    identity: Identity,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "recovery-command-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let prefs = Self::prefs();
        let library = identity::parse(&prefs).unwrap();
        Self {
            root,
            identity: Identity::new("synthetic", &library).unwrap(),
        }
    }
    fn prefs() -> Vec<u8> {
        let mut prefs = vec![0; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4..6].copy_from_slice(&1u16.to_le_bytes());
        prefs[6..8].copy_from_slice(&25u16.to_le_bytes());
        prefs[8] = 1;
        prefs[12..20].copy_from_slice(&1u64.to_le_bytes());
        prefs[20..28].copy_from_slice(&2u64.to_le_bytes());
        prefs
    }
    fn current(&self, anchor: u32) -> Current {
        let prefs = Self::prefs();
        let library = identity::parse(&prefs).unwrap();
        Current {
            device: "synthetic".into(),
            identity: Identity::new("synthetic", &library).unwrap(),
            library,
            catalog: music_protocol::test_support::catalogue::collection_catalog(anchor),
            raw_preferences: prefs.clone(),
            plist_preferences: prefs,
            cdb: Vec::new(),
            artwork_db: None,
            receipt_sha256: [7; 32],
        }
    }
    fn journal(&self) -> Journal {
        Journal::create(&self.root.join("journal"), &self.identity).unwrap()
    }
    fn content(&self) -> Content {
        let source = self.root.join("source.m4a");
        fs::write(
            &source,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        Content {
            source: (music_protocol::media::fingerprint::read(&source).unwrap()).into(),
            track_pid: 1000,
            path: "iTunes_Control/Music/F39/NEWW.m4a".into(),
            artwork: None,
            collection: None,
        }
    }
    fn options<'a>(&'a self, trace: &'a Path, cancel: Option<i64>) -> Options<'a> {
        Options {
            snapshot: &self.root,
            journal: &self.root,
            run_dir: Some(&self.root),
            trace,
            cancel_prepared: cancel,
            selected_device: None,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[tokio::test]
async fn a_prepared_batchs_conversions_go_with_it() {
    let f = Fixture::new();
    // A run that ended left the batch's conversion and one it made ahead.
    let ended = f.root.join("rustunes-flac-4000000000-1");
    fs::DirBuilder::new().mode(0o700).create(&ended).unwrap();
    fs::write(ended.join("sent.m4a"), b"x").unwrap();
    fs::write(ended.join("ahead.m4a"), b"y").unwrap();
    let evidence = f.root.join("batch-001");
    fs::create_dir(&evidence).unwrap();
    let input = serde_json::to_vec(&serde_json::json!({"members":[{
        "source_path":ended.join("sent.m4a"),
        "album":"00000000000007d1","artist":"0000000000000bb9",
    }]}))
    .unwrap();
    fs::write(evidence.join("input.json"), &input).unwrap();
    let conversion = crate::engine::state::journal::Conversion {
        source_path: f.root.join("song.flac"),
        source_bytes: 900,
        source_sha256: [4; 32],
        source_audio: [5; 32],
        ffmpeg: "ffmpeg version 8.1.3".into(),
    };
    let id = test_support::prepare_converted_insert_batch_at(
        &mut f.journal(),
        f.content(),
        conversion,
        &evidence,
        <sha2::Sha256 as sha2::Digest>::digest(&input).into(),
    );
    let journal = f.root.join("journal");
    let batch = Journal::open(&journal, &f.identity)
        .unwrap()
        .insert_batch(id)
        .unwrap();
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let report = resume_insert_batch(&journal, &f.identity, batch, &control)
        .await
        .unwrap();
    assert_eq!(report.document["status"], "cancelled");
    assert!(!ended.exists());
}

/// A saved capture of the collection fixture whose one track has no file
/// location, as a run that met an interrupted insert batch takes it.
fn locationless_snapshot(f: &Fixture) -> PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let path = f.root.join("snapshot");
    for directory in [path.clone(), path.join("catalog")] {
        fs::DirBuilder::new().mode(0o700).create(directory).unwrap();
    }
    let db = path.join("catalog/MediaLibrary.sqlitedb");
    music_protocol::test_support::catalogue::copy_closed_collection_fixture(&db);
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "UPDATE item SET base_location_id=0 WHERE item_pid=1;
            UPDATE item_extra SET location='',file_size=0 WHERE item_pid=1;
            UPDATE item_state SET has_non_purgeable_asset=0 WHERE item_pid=1;",
        )
        .unwrap();
    connection.close().unwrap();
    let catalog = music_protocol::catalogue::sqlite::read_snapshot(&db, None, &f.root).unwrap();
    let prefs = Fixture::prefs();
    let library = identity::parse(&prefs).unwrap();
    let hex = |bytes: &[u8]| music_protocol::protocol::transfer::hex(bytes);
    let hashes = catalog.source_hashes();
    let index = serde_json::json!({
        "schema":"rustunes.device-audio-index","complete":true,
        "identity":{"device_identifier_sha256":hex(&f.identity.device_digest()),
            "library_identity_sha256":hex(&library.binding_sha256())},
        "catalog":{"database_sha256":hex(&hashes.database),"database_bytes":hashes.database_bytes,
            "wal_sha256":null,"wal_bytes":null,"persisted_media_anchor":catalog.persisted_media_anchor()},
        "track_count":0,"total_bytes":0,"alac_track_count":0,"other_track_count":0,
        "records":[],"other_records":[],"unfilled_records":["FFFFFFFFFFFFFFFF"],
        "encoded_audio_kind":"encoded-alac-not-pcm","progress_sha256":hex(&Sha256::digest(b"progress")),
        "completed_unix_time_ns":"1","fresh_prefs_before_after_components_equal":true,
        "atomic_snapshot_claim":false,"catalog_metadata":{},
    });
    for name in [
        "before-iTunesPrefs",
        "before-iTunesPrefs.plist",
        "after-iTunesPrefs",
        "after-iTunesPrefs.plist",
    ] {
        fs::write(path.join(name), &prefs).unwrap();
    }
    fs::write(
        path.join("complete-index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    fs::write(path.join("progress.jsonl"), b"progress").unwrap();
    for file in fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .chain([db])
    {
        if file.is_file() {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    path
}

#[tokio::test]
async fn a_snapshot_with_rows_without_audio_identifies_the_phone_for_a_batch() {
    let f = Fixture::new();
    let snapshot = locationless_snapshot(&f);
    // The strict open refuses it; `resume` opens it with `open_allowing`.
    assert!(snapshot::open(&snapshot).is_err());
    let journal_path = f.root.join("journal");
    let mut journal = f.journal();
    let options = |trace| Options {
        snapshot: &snapshot,
        journal: &journal_path,
        run_dir: None,
        trace,
        cancel_prepared: None,
        selected_device: None,
    };
    // With no batch unfinished, every other route still needs every row's audio.
    let quiet = f.root.join("quiet");
    let error = run(options(&quiet), &crate::engine::test_engine())
        .await
        .err()
        .unwrap();
    assert!(
        format!("{error:#}").contains("holds catalogue rows without audio"),
        "{error:#}"
    );
    let evidence = f.root.join("batch");
    fs::DirBuilder::new().mode(0o700).create(&evidence).unwrap();
    let id = test_support::prepare_insert_batch_at(&mut journal, f.content(), &evidence);
    drop(journal);
    // The batch route takes it: the Prepared batch is cancelled (the
    // helper's synthetic input receipt then stops the cleanup after it).
    let trace = f.root.join("batch-trace");
    let _ = run(options(&trace), &crate::engine::test_engine()).await;
    assert_eq!(
        Journal::open_read_only(&journal_path, &f.identity)
            .unwrap()
            .insert_batch(id)
            .unwrap()
            .state,
        crate::engine::state::journal::InsertBatchState::Cancelled
    );
}
