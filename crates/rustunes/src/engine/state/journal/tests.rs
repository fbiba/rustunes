use super::*;
use plist::Dictionary;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "music-journal-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn journal(&self) -> PathBuf {
        self.0.join("journal")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn identity(device: &str, catalog: u64, user: u64) -> Identity {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25_u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&catalog.to_le_bytes());
    raw[20..28].copy_from_slice(&user.to_le_bytes());
    Identity::new(
        device,
        &music_protocol::catalogue::identity::parse(&raw).unwrap(),
    )
    .unwrap()
}
fn scope() -> Identity {
    identity("test-device", 1, 2)
}

#[test]
fn binding_inventory_excludes_pending_work_and_preserves_ownership() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    assert!(journal.active_verified_bindings().unwrap().is_empty());
    journal.ensure_identity(&scope()).unwrap();
    assert!(
        journal
            .ensure_identity(&identity("other-device", 1, 2))
            .is_err()
    );
    assert!(
        journal
            .ensure_identity(&identity("test-device", 4, 7))
            .is_err()
    );
    let owned = complete_insert(&mut journal, 1);
    let second = complete_insert(&mut journal, 2);
    journal.prepare(insert(3)).unwrap();
    let inventory = journal.active_verified_bindings().unwrap();
    assert_eq!(inventory.len(), 2);
    for content in [owned, second] {
        assert!(inventory.iter().any(|b| b.content == content));
    }
    assert!(
        journal
            .lookup_active_verified(&content(3).source.audio)
            .unwrap()
            .is_none()
    );
}

fn content(seed: u8) -> Content {
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
    Content {
        source: (SourceFingerprint {
            file_bytes: 1200,
            full_file_sha256: [seed; 32],
            audio,
        })
        .into(),
        track_pid: u64::MAX - u64::from(seed),
        path: format!(
            "iTunes_Control/Music/F39/AAA{}.m4a",
            char::from(b'A' + seed)
        ),
        artwork: None,
        collection: Some([42; 32]),
    }
}

fn operation(
    kind: AttemptKind,
    content: &Content,
    anchor: u32,
    album: Option<u64>,
    artist: Option<u64>,
) -> Vec<u8> {
    let mut db_info = Dictionary::new();
    db_info.insert("primary_container_pid".into(), 50_i64.into());
    let mut db = Dictionary::new();
    db.insert("operation".into(), "update_db_info".into());
    db.insert("pid".into(), 51_i64.into());
    db.insert("db_info".into(), Value::Dictionary(db_info));
    let mut item = Dictionary::new();
    if let Some(album) = album {
        item.insert("album_pid".into(), (album as i64).into());
    }
    if let Some(artist) = artist {
        item.insert("artist_pid".into(), (artist as i64).into());
    }
    if let Some(art) = &content.artwork {
        item.insert("artwork_cache_id".into(), art.cache_id.into());
        item.insert("artwork_checksum".into(), (art.bytes + 17).into());
    }
    let mut own = Dictionary::new();
    own.insert(
        "operation".into(),
        match kind {
            AttemptKind::Insert => "insert_track",
            AttemptKind::Artwork => "update_track",
        }
        .into(),
    );
    own.insert("pid".into(), (content.track_pid as i64).into());
    own.insert("item".into(), Value::Dictionary(item));
    let mut root = Dictionary::new();
    root.insert("revision".into(), (anchor + 1).into());
    root.insert("timestamp".into(), Value::Date(UNIX_EPOCH.into()));
    root.insert(
        "operations".into(),
        Value::Array(vec![Value::Dictionary(db), Value::Dictionary(own)]),
    );
    let mut bytes = Vec::new();
    Value::Dictionary(root)
        .to_writer_binary(&mut bytes)
        .unwrap();
    bytes
}
fn insert(seed: u8) -> PreparedAttempt {
    let content = content(seed);
    let album = Some(1000 + u64::from(seed));
    let artist = Some(2000 + u64::from(seed));
    PreparedAttempt {
        operation: operation(AttemptKind::Insert, &content, 13, album, artist),
        content,
        kind: AttemptKind::Insert,
        new_album_pid: album,
        new_artist_pid: artist,
        current_media_anchor: 13,
        input_receipt_sha256: [7; 32],
    }
}
fn start(anchor: u32) -> StartEvidence {
    StartEvidence {
        ready_media_anchor: anchor,
        ready_receipt_sha256: [8; 32],
        signature: [1; 21],
    }
}
fn verified(content: &Content, anchor: u32) -> CompletionEvidence {
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
fn complete_insert(journal: &mut Journal, seed: u8) -> Content {
    let prepared = insert(seed);
    let content = prepared.content.clone();
    let id = journal.prepare(prepared).unwrap();
    let permit = journal.mark_started(id, start(13)).unwrap();
    journal.complete(permit, verified(&content, 14)).unwrap();
    content
}

#[test]
fn private_creation_reopen_and_exact_preparation() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    assert_eq!(
        fs::metadata(temp.journal()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(temp.journal().join(DB_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let prepared = insert(1);
    let expected = prepared.operation.clone();
    let source = prepared.content.source.clone();
    let id = journal.prepare(prepared).unwrap();
    drop(journal);
    let journal = Journal::open(&temp.journal(), &scope()).unwrap();
    let outstanding = journal.outstanding().unwrap().unwrap();
    assert_eq!(outstanding.id, id);
    assert_eq!(outstanding.state, AttemptState::Prepared);
    assert_eq!(outstanding.prepared.operation, expected);
    assert!(outstanding.prepared.content.source == source);
    assert_eq!(
        journal.reserved_pids().unwrap(),
        BTreeSet::from([u64::MAX - 1, 1001, 2001])
    );
    assert!(
        journal
            .lookup_active_verified(&source.audio)
            .unwrap()
            .is_none()
    );
    assert!(Journal::create(&temp.journal(), &scope()).is_err());
}

fn read_only_files(
    directory: &Path,
) -> std::collections::BTreeMap<String, (Vec<u8>, u32, i64, i64)> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let m = fs::symlink_metadata(&path).unwrap();
            assert!(m.is_file());
            (
                path.file_name().unwrap().to_str().unwrap().to_owned(),
                (
                    fs::read(&path).unwrap(),
                    m.permissions().mode(),
                    m.mtime(),
                    m.mtime_nsec(),
                ),
            )
        })
        .collect()
}

#[test]
fn read_only_open_preserves_quiescent_bytes_and_refuses_transitions() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let content = complete_insert(&mut journal, 1);
    drop(journal);
    let before = read_only_files(&temp.journal());
    let mut reader = Journal::open_read_only(&temp.journal(), &scope()).unwrap();
    reader.ensure_identity(&scope()).unwrap();
    assert_eq!(reader.active_verified_bindings().unwrap().len(), 1);
    assert!(
        reader
            .lookup_active_verified(&content.source.audio)
            .unwrap()
            .is_some()
    );
    assert!(reader.outstanding().unwrap().is_none());
    let copy_path = reader._read_only_copy.as_ref().unwrap().directory.clone();
    assert_eq!(
        fs::metadata(&copy_path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(copy_path.join(DB_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(reader.prepare(insert(2)).is_err());
    assert_eq!(reader.active_verified_bindings().unwrap().len(), 1);
    drop(reader);
    assert!(!copy_path.exists());
    assert_eq!(read_only_files(&temp.journal()), before);
    assert!(Journal::open_read_only(&temp.journal(), &identity("other", 1, 2)).is_err());
    assert_eq!(read_only_files(&temp.journal()), before);
}

#[test]
fn read_only_clone_isolates_wal_transition_at_sqlite_boundary() {
    for force_clone_wal in [false, true] {
        let temp = Temp::new();
        drop(Journal::create(&temp.journal(), &scope()).unwrap());
        let directory = private_directory(&temp.journal()).unwrap();
        let database = File::open(temp.journal().join(DB_NAME)).unwrap();
        let before = database.metadata().unwrap();
        require_quiescent_read_only(&temp.journal(), &database).unwrap();
        let copy = ReadOnlyCopy::new(&database, before.len()).unwrap();

        // This deterministic seam is exactly where a separate writer can
        // change mode after source checks but before the SQLite connection.
        let writer = Connection::open(temp.journal().join(DB_NAME)).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        drop(writer);
        let after_writer = read_only_files(&temp.journal());
        assert_eq!(after_writer.len(), 1);
        assert_eq!(after_writer[DB_NAME].0[18], 2);

        if force_clone_wal {
            // Even a WAL header reaching the SQLite-facing helper may affect
            // only the disposable copy. Never pass the original path to it.
            OpenOptions::new()
                .write(true)
                .open(copy.directory.join(DB_NAME))
                .unwrap()
                .write_all_at(&[2, 2], 18)
                .unwrap();
            assert!(copy.connect().is_err());
        } else {
            let connection = copy.connect().unwrap();
            validate_schema(&connection).unwrap();
        }
        assert!(
            require_source_unchanged(
                &temp.journal(),
                &directory,
                &database,
                &before,
                Some(copy.sha256)
            )
            .is_err()
        );
        drop(copy);
        assert_eq!(read_only_files(&temp.journal()), after_writer);
    }
}

#[test]
fn read_only_snapshot_is_pinned_and_refuses_replaced_source_or_oversize() {
    let temp = Temp::new();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    let reader = Journal::open_read_only(&temp.journal(), &scope()).unwrap();
    let directory = private_directory(&temp.journal()).unwrap();
    let database = File::open(temp.journal().join(DB_NAME)).unwrap();
    let before = database.metadata().unwrap();
    let copy = reader._read_only_copy.as_ref().unwrap();
    let mut writer = Journal::open(&temp.journal(), &scope()).unwrap();
    writer.prepare(insert(1)).unwrap();
    drop(writer);
    assert!(reader.outstanding().unwrap().is_none());
    assert!(
        require_source_unchanged(
            &temp.journal(),
            &directory,
            &database,
            &before,
            Some(copy.sha256)
        )
        .is_err()
    );
    drop(reader);

    let moved = temp.0.join("moved-journal");
    fs::rename(temp.journal(), &moved).unwrap();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    assert!(
        require_source_unchanged(
            &temp.journal(),
            &directory,
            &database,
            &before,
            Some([1; 32])
        )
        .is_err()
    );
    let large = OpenOptions::new()
        .write(true)
        .open(temp.journal().join(DB_NAME))
        .unwrap();
    large.set_len(MAX_READ_ONLY_DATABASE + 1).unwrap();
    assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
    assert_eq!(large.metadata().unwrap().len(), MAX_READ_ONLY_DATABASE + 1);
}

#[test]
fn read_only_open_refuses_wal_with_or_without_sidecars_without_mode_changes() {
    let temp = Temp::new();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    let path = temp.journal().join(DB_NAME);
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE scope SET nonce=randomblob(32)").unwrap();
    let with_sidecars = read_only_files(&temp.journal());
    assert!(with_sidecars.contains_key("journal.sqlite-wal"));
    assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
    assert_eq!(read_only_files(&temp.journal()), with_sidecars);
    drop(connection);
    let without_sidecars = read_only_files(&temp.journal());
    assert_eq!(without_sidecars.len(), 1);
    assert_eq!(without_sidecars[DB_NAME].0[18], 2);
    assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
    assert_eq!(read_only_files(&temp.journal()), without_sidecars);
}

#[test]
fn read_only_open_refuses_even_empty_recovery_sidecars_without_removing_them() {
    let temp = Temp::new();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    for suffix in ["-journal", "-wal", "-shm"] {
        let path = temp.journal().join(format!("{DB_NAME}{suffix}"));
        fs::write(&path, []).unwrap();
        let before = read_only_files(&temp.journal());
        assert!(Journal::open_read_only(&temp.journal(), &scope()).is_err());
        assert_eq!(read_only_files(&temp.journal()), before);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn read_only_hot_rollback_child() {
    let Some(path) = std::env::var_os("MUSIC_JOURNAL_READ_ONLY_HOT_TEST_DIRECTORY") else {
        return;
    };
    let path = Path::new(&path);
    drop(Journal::create(path, &scope()).unwrap());
    let connection = Connection::open(path.join(DB_NAME)).unwrap();
    connection.execute_batch("PRAGMA cache_size=1; PRAGMA cache_spill=ON; PRAGMA synchronous=FULL; BEGIN IMMEDIATE; UPDATE scope SET nonce=randomblob(32)").unwrap();
    for value in 1_u64..=1000 {
        connection
            .execute(
                "INSERT INTO reservations(namespace,value,attempt,consumed) VALUES(1,?1,NULL,0)",
                [value.to_be_bytes().as_slice()],
            )
            .unwrap();
    }
    // Force a real interrupted rollback journal: no Connection destructor,
    // rollback or cleanup can run in this controlled temporary child.
    std::process::exit(74);
}

#[test]
fn read_only_open_refuses_actual_hot_rollback_without_recovery() {
    let temp = Temp::new();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            concat!(module_path!(), "::read_only_hot_rollback_child")
                .split_once("::")
                .unwrap()
                .1,
        ])
        .env("MUSIC_JOURNAL_READ_ONLY_HOT_TEST_DIRECTORY", temp.journal())
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(74),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let before = read_only_files(&temp.journal());
    let rollback = &before["journal.sqlite-journal"].0;
    assert!(rollback.len() > 512);
    assert_eq!(
        &rollback[..8],
        &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
    );
    let error = Journal::open_read_only(&temp.journal(), &scope())
        .err()
        .unwrap();
    assert!(error.to_string().contains("recovery sidecar"));
    assert_eq!(read_only_files(&temp.journal()), before);
}

#[test]
fn device_library_schema_and_permissions_mismatch_are_refused() {
    let temp = Temp::new();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    assert!(Journal::open(&temp.journal(), &identity("other-device", 1, 2)).is_err());
    // Same wire XOR, different full persisted components.
    assert!(Journal::open(&temp.journal(), &identity("test-device", 4, 7)).is_err());
    let path = temp.journal().join(DB_NAME);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Journal::open(&temp.journal(), &scope()).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let database = Connection::open(&path).unwrap();
    database
        .execute_batch("DROP TRIGGER immutable_preparation")
        .unwrap();
    drop(database);
    assert!(Journal::open(&temp.journal(), &scope()).is_err());
}

#[test]
fn symlinks_and_hardlinks_are_not_journal_files() {
    let temp = Temp::new();
    drop(Journal::create(&temp.journal(), &scope()).unwrap());
    std::os::unix::fs::symlink(temp.journal(), temp.0.join("linked")).unwrap();
    assert!(Journal::open(&temp.0.join("linked"), &scope()).is_err());
    fs::hard_link(temp.journal().join(DB_NAME), temp.0.join("hardlink")).unwrap();
    assert!(Journal::open(&temp.journal(), &scope()).is_err());
}

#[test]
fn cancellation_retains_ids_and_destination_and_collision_rolls_back() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let id = journal.prepare(insert(1)).unwrap();
    journal.cancel_prepared(id).unwrap();
    assert!(
        journal
            .reserved_paths()
            .unwrap()
            .contains(&content(1).path.to_ascii_lowercase())
    );
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Cancelled);
    assert!(journal.outstanding().unwrap().is_none());
    assert!(journal.prepare(insert(1)).is_err());
    let mut other = insert(2);
    other.new_album_pid = Some(1001);
    other.operation = operation(
        other.kind,
        &other.content,
        13,
        other.new_album_pid,
        other.new_artist_pid,
    );
    assert!(journal.prepare(other).is_err());
    assert!(!journal.reserved_pids().unwrap().contains(&(u64::MAX - 2)));
    let mut other = insert(2);
    other.content.path = content(1)
        .path
        .to_lowercase()
        .replace("itunes_control/music/f", "iTunes_Control/Music/F");
    assert!(journal.prepare(other).is_err());
    assert!(journal.outstanding().unwrap().is_none());
    let next = journal.prepare(insert(2)).unwrap();
    assert_ne!(next, id);
}

#[test]
fn only_one_outstanding_across_independent_connections() {
    let temp = Temp::new();
    let mut first = Journal::create(&temp.journal(), &scope()).unwrap();
    let mut second = Journal::open(&temp.journal(), &scope()).unwrap();
    let id = first.prepare(insert(1)).unwrap();
    assert!(second.prepare(insert(2)).is_err());
    first.cancel_prepared(id).unwrap();
    second.prepare(insert(2)).unwrap();
    assert!(first.prepare(insert(3)).is_err());
}

#[test]
fn interrupted_started_requires_reconciliation_and_never_reissues_permit() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let prepared = insert(1);
    let content = prepared.content.clone();
    let operation = prepared.operation.clone();
    let id = journal.prepare(prepared).unwrap();
    assert!(journal.mark_started(id, start(12)).is_err());
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
    let permit = journal.mark_started(id, start(13)).unwrap();
    assert_eq!(permit.operation(), operation);
    assert_eq!(permit.signature(), &[1; 21]);
    drop(permit);
    drop(journal);
    let mut journal = Journal::open(&temp.journal(), &scope()).unwrap();
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
    assert!(journal.mark_started(id, start(13)).is_err());
    assert!(journal.cancel_prepared(id).is_err());
    assert!(journal.prepare(insert(2)).is_err());
    assert!(
        journal
            .reconcile_completed(id, verified(&content, 13))
            .is_err()
    );
    let mut wrong = verified(&content, 14);
    wrong.audio_file_sha256 = [99; 32];
    assert!(journal.reconcile_completed(id, wrong).is_err());
    assert!(
        journal
            .lookup_active_verified(&content.source.audio)
            .unwrap()
            .is_none()
    );
    journal
        .reconcile_completed(id, verified(&content, 15))
        .unwrap();
    assert!(journal.outstanding().unwrap().is_none());
    let binding = journal
        .lookup_active_verified(&content.source.audio)
        .unwrap()
        .unwrap();
    assert_eq!(binding.media_anchor, 15);
    assert!(
        journal
            .reconcile_completed(id, verified(&content, 15))
            .is_err()
    );
}

#[test]
fn not_applied_settles_only_started_and_keeps_reservations() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let prepared = insert(1);
    let content = prepared.content.clone();
    let id = journal.prepare(prepared).unwrap();
    assert!(journal.mark_not_applied(id, [7; 32]).is_err());
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
    drop(journal.mark_started(id, start(13)).unwrap());
    assert!(journal.mark_not_applied(id, [0; 32]).is_err());
    journal.mark_not_applied(id, [7; 32]).unwrap();
    drop(journal);
    let mut journal = Journal::open(&temp.journal(), &scope()).unwrap();
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::NotApplied);
    assert!(journal.outstanding().unwrap().is_none());
    assert!(journal.start_evidence(id).unwrap().is_some());
    assert!(
        journal
            .lookup_active_verified(&content.source.audio)
            .unwrap()
            .is_none()
    );
    assert!(
        journal
            .reserved_paths()
            .unwrap()
            .contains(&content.path.to_ascii_lowercase())
    );
    assert!(journal.prepare(insert(1)).is_err());
    assert!(journal.mark_not_applied(id, [7; 32]).is_err());
    assert!(
        journal
            .reconcile_completed(id, verified(&content, 14))
            .is_err()
    );
    for sql in [
        "UPDATE attempts SET state=1 WHERE id=?1",
        "UPDATE attempts SET verified_anchor=14 WHERE id=?1",
    ] {
        assert!(journal.connection.execute(sql, [id.0]).is_err());
    }
    journal.prepare(insert(2)).unwrap();
}

#[test]
fn abandoned_settles_only_a_started_cover_past_its_anchor() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let base = complete_insert(&mut journal, 1);
    let mut with_art = base.clone();
    with_art.artwork = Some(Artwork {
        cache_id: 300,
        bytes: 100,
        sha256: [5; 32],
    });
    let id = journal
        .prepare(PreparedAttempt {
            kind: AttemptKind::Artwork,
            content: with_art.clone(),
            new_album_pid: None,
            new_artist_pid: None,
            current_media_anchor: 14,
            operation: operation(AttemptKind::Artwork, &with_art, 14, None, None),
            input_receipt_sha256: [9; 32],
        })
        .unwrap();
    assert!(journal.mark_abandoned(id, [7; 32], 15).is_err());
    drop(journal.mark_started(id, start(14)).unwrap());
    for (receipt, anchor) in [([0; 32], 15), ([7; 32], 14)] {
        assert!(journal.mark_abandoned(id, receipt, anchor).is_err());
    }
    journal.mark_abandoned(id, [7; 32], 15).unwrap();
    drop(journal);
    let mut journal = Journal::open(&temp.journal(), &scope()).unwrap();
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Abandoned);
    assert!(journal.outstanding().unwrap().is_none());
    assert!(journal.start_evidence(id).unwrap().is_some());
    // The binding keeps its last verified content; the cache ID stays reserved.
    let binding = journal
        .lookup_active_verified(&base.source.audio)
        .unwrap()
        .unwrap();
    assert!(binding.content.artwork.is_none());
    assert!(journal.reserved_cache_ids().unwrap().contains(&300));
    for sql in [
        "UPDATE attempts SET state=1 WHERE id=?1",
        "UPDATE attempts SET verified_anchor=NULL WHERE id=?1",
    ] {
        assert!(journal.connection.execute(sql, [id.0]).is_err());
    }
    // An insert cannot be abandoned.
    let insert_id = journal.prepare(insert(2)).unwrap();
    drop(journal.mark_started(insert_id, start(13)).unwrap());
    assert!(journal.mark_abandoned(insert_id, [7; 32], 16).is_err());
}

#[test]
fn completion_is_atomic_and_exact_and_keeps_consumed_ids() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let own = complete_insert(&mut journal, 1);
    assert!(journal.prepare(insert(1)).is_err());
    let consumed: i64 = journal
        .connection
        .query_row(
            "SELECT COUNT(*) FROM reservations WHERE consumed=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(consumed, 4);
    drop(journal);
    let journal = Journal::open(&temp.journal(), &scope()).unwrap();
    let binding = journal
        .lookup_active_verified(&own.source.audio)
        .unwrap()
        .unwrap();
    assert!(binding.content == own);
    assert_eq!(binding.receipt_sha256, [9; 32]);
}

#[test]
fn failed_completion_consumes_permit_but_leaves_started_and_no_binding() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let prepared = insert(1);
    let own = prepared.content.clone();
    let id = journal.prepare(prepared).unwrap();
    let permit = journal.mark_started(id, start(13)).unwrap();
    let mut wrong = verified(&own, 14);
    wrong.path = content(2).path;
    assert!(journal.complete(permit, wrong).is_err());
    assert!(
        journal
            .lookup_active_verified(&own.source.audio)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
    assert!(journal.mark_started(id, start(13)).is_err());
}

#[test]
fn prepared_bytes_cannot_be_mutated_and_delete_commands_are_refused() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let id = journal.prepare(insert(1)).unwrap();
    assert!(
        journal
            .connection
            .execute("UPDATE attempts SET operation=x'00' WHERE id=?1", [id.0])
            .is_err()
    );
    journal.cancel_prepared(id).unwrap();
    let mut prepared = insert(2);
    let mut value = Value::from_reader(Cursor::new(&prepared.operation)).unwrap();
    value
        .as_dictionary_mut()
        .unwrap()
        .get_mut("operations")
        .unwrap()
        .as_array_mut()
        .unwrap()[1]
        .as_dictionary_mut()
        .unwrap()
        .insert("operation".into(), "delete_track".into());
    prepared.operation.clear();
    value.to_writer_binary(&mut prepared.operation).unwrap();
    assert!(journal.prepare(prepared).is_err());
    assert!(journal.outstanding().unwrap().is_none());
}

#[test]
fn artwork_updates_keep_the_binding_and_cache_ids_stay_reserved() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let original = content(1);
    test_support::insert_verified_with(&mut journal, original.clone(), verified(&original, 13));
    assert!(journal.prepare(insert(1)).is_err());
    let mut with_art = original.clone();
    with_art.artwork = Some(Artwork {
        cache_id: 677,
        bytes: 187306,
        sha256: [10; 32],
    });
    let prepared = PreparedAttempt {
        kind: AttemptKind::Artwork,
        content: with_art.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: 13,
        operation: operation(AttemptKind::Artwork, &with_art, 13, None, None),
        input_receipt_sha256: [7; 32],
    };
    let id = journal.prepare(prepared).unwrap();
    let permit = journal.mark_started(id, start(13)).unwrap();
    journal.complete(permit, verified(&with_art, 14)).unwrap();
    let binding = journal
        .lookup_active_verified(&original.source.audio)
        .unwrap()
        .unwrap();
    assert!(binding.content == with_art);
    assert_eq!(journal.reserved_cache_ids().unwrap(), BTreeSet::from([677]));
    let mut next = with_art.clone();
    next.artwork.as_mut().unwrap().cache_id = 678;
    let next = PreparedAttempt {
        kind: AttemptKind::Artwork,
        content: next.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: 14,
        operation: operation(AttemptKind::Artwork, &next, 14, None, None),
        input_receipt_sha256: [7; 32],
    };
    let id = journal.prepare(next).unwrap();
    journal.cancel_prepared(id).unwrap();
    assert_eq!(
        journal.reserved_cache_ids().unwrap(),
        BTreeSet::from([677, 678])
    );
    assert_eq!(
        journal
            .lookup_active_verified(&original.source.audio)
            .unwrap()
            .unwrap()
            .content
            .artwork
            .unwrap()
            .cache_id,
        677
    );
}

#[test]
fn content_bounds_and_fingerprint_consistency_are_checked_before_transaction() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let mut prepared = insert(1);
    crate::transferred_fixture::mutate_alac(&mut prepared.content.source, |audio| {
        audio.media_sha256 = [99; 32];
    });
    assert!(journal.prepare(prepared).is_err());
    let mut prepared = insert(1);
    prepared.content.path = "iTunes_Control/Music/F39/../evil".into();
    assert!(journal.prepare(prepared).is_err());
    let mut prepared = insert(1);
    prepared.operation = vec![0; MAX_OPERATION + 1];
    assert!(journal.prepare(prepared).is_err());
    let mut prepared = insert(1);
    prepared.input_receipt_sha256 = [0; 32];
    assert!(journal.prepare(prepared).is_err());
    assert!(journal.outstanding().unwrap().is_none());
    assert!(journal.reserved_pids().unwrap().is_empty());
    let encoded = encode_content(&content(1)).unwrap();
    for length in [0, 1, 189, encoded.len() - 1] {
        assert!(decode_content(&encoded[..length]).is_err());
    }
    let mut extended = encoded;
    extended.push(0);
    assert!(decode_content(&extended).is_err());
}

#[test]
fn a_permit_cannot_complete_a_different_journal() {
    let one = Temp::new();
    let two = Temp::new();
    let mut first = Journal::create(&one.journal(), &scope()).unwrap();
    let mut second = Journal::create(&two.journal(), &scope()).unwrap();
    let first_id = first.prepare(insert(1)).unwrap();
    let second_id = second.prepare(insert(1)).unwrap();
    let permit = first.mark_started(first_id, start(13)).unwrap();
    drop(second.mark_started(second_id, start(13)).unwrap());
    assert!(second.complete(permit, verified(&content(1), 14)).is_err());
    assert_eq!(
        second.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
}

#[test]
fn abrupt_exit_child() {
    // The normal focused run performs no action here. The parent test invokes
    // only this test with an exact private temporary destination.
    let Some(path) = std::env::var_os("MUSIC_JOURNAL_EXIT_TEST_DIRECTORY") else {
        return;
    };
    let mut journal = Journal::create(Path::new(&path), &scope()).unwrap();
    let id = journal.prepare(insert(1)).unwrap();
    let _permit = journal.mark_started(id, start(13)).unwrap();
    std::process::exit(73); // No Rust destructors/Connection::drop execute.
}

#[test]
fn abrupt_process_exit_preserves_started_without_retry_permission() {
    let temp = Temp::new();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            concat!(module_path!(), "::abrupt_exit_child")
                .split_once("::")
                .unwrap()
                .1,
        ])
        .env("MUSIC_JOURNAL_EXIT_TEST_DIRECTORY", temp.journal())
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut journal = Journal::open(&temp.journal(), &scope()).unwrap();
    let attempt = journal.outstanding().unwrap().unwrap();
    assert_eq!(attempt.state, AttemptState::ReconciliationRequired);
    assert_eq!(journal.reserved_pids().unwrap().len(), 3);
    assert!(journal.mark_started(attempt.id, start(13)).is_err());
    assert!(journal.prepare(insert(2)).is_err());
}

#[test]
fn transaction_fault_rolls_back_binding_consumption_and_completion_together() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let prepared = insert(1);
    let own = prepared.content.clone();
    let id = journal.prepare(prepared).unwrap();
    let permit = journal.mark_started(id, start(13)).unwrap();
    // Fail after finish() has inserted the binding and consumed reservations,
    // before the final state update. All three effects must roll back together.
    journal.connection.execute_batch("CREATE TRIGGER completion_fault BEFORE UPDATE OF state ON attempts WHEN NEW.state=2 BEGIN SELECT RAISE(ABORT,'injected completion failure'); END;").unwrap();
    assert!(journal.complete(permit, verified(&own, 14)).is_err());
    assert!(
        journal
            .lookup_active_verified(&own.source.audio)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        journal
            .connection
            .query_row(
                "SELECT COUNT(*) FROM reservations WHERE consumed=1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
    journal
        .connection
        .execute_batch("DROP TRIGGER completion_fault")
        .unwrap();
    drop(journal);
    let mut journal = Journal::open(&temp.journal(), &scope()).unwrap();
    journal.reconcile_completed(id, verified(&own, 14)).unwrap();
    assert!(
        journal
            .lookup_active_verified(&own.source.audio)
            .unwrap()
            .is_some()
    );
}

#[test]
fn artwork_preserves_transferred_container_despite_desired_source_tag_changes() {
    let temp = Temp::new();
    let mut journal = Journal::create(&temp.journal(), &scope()).unwrap();
    let original = complete_insert(&mut journal, 1);
    let mut desired = original.clone();
    desired.source.full_file_sha256 = [99; 32]; // Same encoded audio, new tags.
    desired.source.file_bytes += 100;
    desired.artwork = Some(Artwork {
        cache_id: 677,
        bytes: 5000,
        sha256: [10; 32],
    });
    let prepared = PreparedAttempt {
        kind: AttemptKind::Artwork,
        content: desired.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: 14,
        operation: operation(AttemptKind::Artwork, &desired, 14, None, None),
        input_receipt_sha256: [7; 32],
    };
    assert!(journal.prepare(prepared).is_err());
    assert!(journal.outstanding().unwrap().is_none());
    desired.source = original.source.clone();
    let prepared = PreparedAttempt {
        kind: AttemptKind::Artwork,
        content: desired.clone(),
        new_album_pid: None,
        new_artist_pid: None,
        current_media_anchor: 14,
        operation: operation(AttemptKind::Artwork, &desired, 14, None, None),
        input_receipt_sha256: [7; 32],
    };
    let id = journal.prepare(prepared).unwrap();
    let permit = journal.mark_started(id, start(14)).unwrap();
    journal.complete(permit, verified(&desired, 15)).unwrap();
    let binding = journal
        .lookup_active_verified(&original.source.audio)
        .unwrap()
        .unwrap();
    assert!(binding.content.source == original.source);
    assert_eq!(binding.content.artwork.unwrap().sha256, [10; 32]);
}

mod bindings_tests;

mod mpeg_tests;

mod carry_tests;
