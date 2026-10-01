use super::*;

#[test]
fn saved_memory_snapshot_matches_real_legacy_catalog_and_wal_without_source_mutation() {
    for has_wal in [false, true] {
        let fixture = Fixture::new(has_wal);
        let wal = fixture.database.with_file_name("fixture.db-wal");
        let before_database = fs::read(&fixture.database).unwrap();
        let before_wal = has_wal.then(|| fs::read(&wal).unwrap());
        let before_entries = fs::read_dir(&fixture.directory.0).unwrap().count();
        let old = fixture.read(has_wal).unwrap();
        let new = read_snapshot_memory_with_control(
            &fixture.database,
            has_wal.then_some(wal.as_path()),
            &fixture.directory.0,
            Instant::now() + Duration::from_secs(30),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(old.tracks(), new.tracks());
        assert!(old.albums == new.albums && old.artists == new.artists);
        assert_eq!(old.database_pid(), new.database_pid());
        assert_eq!(old.primary_container_pid(), new.primary_container_pid());
        assert_eq!(old.persisted_media_anchor(), new.persisted_media_anchor());
        assert_eq!(old.source_hashes().database, new.source_hashes().database);
        assert_eq!(
            old.source_hashes().database_bytes,
            new.source_hashes().database_bytes
        );
        assert_eq!(old.source_hashes().wal, new.source_hashes().wal);
        assert_eq!(old.source_hashes().wal_bytes, new.source_hashes().wal_bytes);
        assert_eq!(fs::read(&fixture.database).unwrap(), before_database);
        assert_eq!(has_wal.then(|| fs::read(&wal).unwrap()), before_wal);
        assert_eq!(
            fs::read_dir(&fixture.directory.0).unwrap().count(),
            before_entries
        );
    }
}

#[test]
fn saved_memory_input_admission_keeps_legacy_caps_while_current_stays_smaller() {
    let fixture = Fixture::new(false);
    let path = fixture.directory.0.join("sparse-admission-only");
    let file = File::create(&path).unwrap();
    // This is a sparse metadata-admission probe, not a large catalogue copy.
    // source_file is the actual admission used by private copying and rehashing.
    for wal in [false, true] {
        for bytes in [
            32 * 1024 * 1024,
            32 * 1024 * 1024 + 1,
            64 * 1024 * 1024,
            64 * 1024 * 1024 + 1,
            256 * 1024 * 1024,
            256 * 1024 * 1024 + 1,
        ] {
            file.set_len(bytes).unwrap();
            for profile in [
                SnapshotStorage::Legacy,
                SnapshotStorage::MemoryTemp,
                SnapshotStorage::Current,
            ] {
                let limits = profile.input_limits();
                let limit = if wal { limits.1 } else { limits.0 };
                let admitted = source_file(&path, limit);
                let expected = match profile {
                    SnapshotStorage::Current => bytes <= 32 * 1024 * 1024,
                    _ if wal => bytes <= 256 * 1024 * 1024,
                    _ => bytes <= 64 * 1024 * 1024,
                };
                assert_eq!(admitted.is_ok(), expected, "wal={wal},bytes={bytes}");
                if let Ok((opened, metadata)) = admitted {
                    assert_eq!(opened.metadata().unwrap().len(), bytes);
                    assert_eq!(metadata.len(), bytes);
                }
            }
        }
    }
    drop(file);
    fs::remove_file(path).unwrap();
}

#[test]
fn saved_memory_control_refuses_before_scratch_or_source_access() {
    let fixture = Fixture::new(false);
    let before = fs::read_dir(&fixture.directory.0).unwrap().count();
    for expired in [false, true] {
        let deadline = if expired {
            Instant::now() - Duration::from_secs(1)
        } else {
            Instant::now() + Duration::from_secs(5)
        };
        let error = read_snapshot_memory_with_control(
            &fixture.directory.0.join("absent.db"),
            None,
            &fixture.directory.0,
            deadline,
            Arc::new(AtomicBool::new(!expired)),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains(if expired { "deadline" } else { "cancelled" })
        );
        assert_eq!(fs::read_dir(&fixture.directory.0).unwrap().count(), before);
    }
}

#[test]
fn saved_memory_actual_connection_acknowledges_temp_storage_and_retains_control() {
    let fixture = Fixture::new(false);
    let before = fs::read(&fixture.database).unwrap();
    let control = ReadControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (connection, phase) = open_readonly_with_control_options(
        &fixture.database,
        &control,
        SnapshotStorage::MemoryTemp.memory_temp(),
    )
    .unwrap();
    assert_eq!(phase.deadline, control.deadline);
    assert!(Arc::ptr_eq(&phase.cancelled, &control.cancelled));
    let mut statement = connection
        .prepare("SELECT item_pid FROM item ORDER BY item_pid")
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(!rows.is_empty());
    drop(statement);
    // Production already checks this before installing its restrictive authorizer.
    // Disable only this test connection's authorizer to inspect the same connection.
    connection
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    let actual: i64 = connection
        .query_row("PRAGMA temp_store", [], |row| row.get(0))
        .unwrap();
    assert_eq!(actual, 2);
    control.cancelled.store(true, Ordering::Relaxed);
    assert!(phase.check().is_err());
    drop(connection);
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
}

#[test]
fn saved_memory_snapshot_rejects_real_corrupt_wal_and_cleans_only_private_copy() {
    let (fixture, mut bytes, _) = reset_wal_fixture();
    bytes[32 + 24] ^= 1;
    let wal = fixture.directory.0.join("memory-temp-corrupt-wal");
    fs::write(&wal, &bytes).unwrap();
    let before_database = fs::read(&fixture.database).unwrap();
    let before_entries = fs::read_dir(&fixture.directory.0).unwrap().count();
    let error = read_snapshot_memory_with_control(
        &fixture.database,
        Some(&wal),
        &fixture.directory.0,
        Instant::now() + Duration::from_secs(30),
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("WAL frame checksum differs"));
    assert_eq!(fs::read(&fixture.database).unwrap(), before_database);
    assert_eq!(fs::read(&wal).unwrap(), bytes);
    assert_eq!(
        fs::read_dir(&fixture.directory.0).unwrap().count(),
        before_entries
    );
    fs::remove_file(wal).unwrap();
}
