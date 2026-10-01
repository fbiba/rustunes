use super::*;
use crate::catalogue::sqlite::{
    Catalog, MAX_DATABASE_BYTES, MAX_WAL_BYTES, SourceHashes, hash_file, open_readonly,
    parse_catalog, read_snapshot, read_snapshot_current_with_control,
    read_snapshot_memory_with_control, read_snapshot_with_control, validate_container,
};
use crate::test_support::catalogue::Fixture;
use std::{
    os::unix::fs::symlink,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

fn control() -> ReadControl {
    ReadControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn copy_fixture(scratch: &mut Scratch, fixture: &Fixture, wal: bool) -> Result<SourceHashes> {
    let db = scratch.copy(&fixture.database, DB, MAX_DATABASE_BYTES, None)?;
    let journal = wal
        .then(|| {
            scratch.copy(
                &fixture.database.with_file_name("fixture.db-wal"),
                WAL,
                MAX_WAL_BYTES,
                None,
            )
        })
        .transpose()?;
    let wal_path = scratch.path().join(WAL);
    validate_container(&scratch.path().join(DB), wal.then_some(wal_path.as_path()))?;
    scratch.prepare_sql(journal.map(|(_, bytes)| bytes), None)?;
    Ok(SourceHashes {
        database: db.0,
        database_bytes: db.1,
        wal: journal.map(|(hash, _)| hash),
        wal_bytes: journal.map(|(_, bytes)| bytes),
    })
}

fn parse_copy(scratch: &Scratch, hashes: SourceHashes) -> Result<Catalog> {
    let connection = open_readonly(&scratch.path().join(DB))?;
    scratch.check(None)?;
    let catalog = parse_catalog(&connection, hashes, None)?;
    scratch.check(None)?;
    Ok(catalog)
}

fn no_scratch_entries(parent: &Path) {
    assert!(fs::read_dir(parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("catalog-sqlite-")
    }));
}

fn checkpoint_without_wal(fixture: &mut Fixture) {
    fixture
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let connection = std::mem::replace(
        &mut fixture.connection,
        rusqlite::Connection::open_in_memory().unwrap(),
    );
    connection.close().unwrap();
    assert!(!fixture.database.with_file_name("fixture.db-wal").exists());
    assert_eq!(&fs::read(&fixture.database).unwrap()[18..20], &[2, 2]);
}

#[test]
fn held_parent_reads_all_storage_modes_without_changing_inputs() {
    // Includes a WAL-only row and a fully checkpointed WAL-mode database whose
    // WAL is absent. The latter still causes SQLite to create private sidecars.
    for kind in 0..4 {
        let mut fixture = Fixture::new(kind == 1 || kind == 3);
        let wal = fixture.database.with_file_name("fixture.db-wal");
        if kind == 1 {
            fixture
                .connection
                .execute_batch("UPDATE item_extra SET title='Held WAL-only title';")
                .unwrap();
        } else if kind == 2 {
            fs::write(&wal, []).unwrap();
        } else if kind == 3 {
            checkpoint_without_wal(&mut fixture);
        }
        let supplied_wal = matches!(kind, 1 | 2).then_some(wal.as_path());
        let db_before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
        let wal_before = supplied_wal.map(|path| hash_file(path, MAX_WAL_BYTES).unwrap());
        let parent = File::open(&fixture.directory.0).unwrap();
        let held_path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
        let policy = control();
        for mode in 0..4 {
            let read = |scratch: &Path| match mode {
                0 => read_snapshot(&fixture.database, supplied_wal, scratch),
                1 => read_snapshot_with_control(
                    &fixture.database,
                    supplied_wal,
                    scratch,
                    policy.deadline,
                    policy.cancelled.clone(),
                ),
                2 => read_snapshot_memory_with_control(
                    &fixture.database,
                    supplied_wal,
                    scratch,
                    policy.deadline,
                    policy.cancelled.clone(),
                ),
                _ => read_snapshot_current_with_control(
                    &fixture.database,
                    supplied_wal,
                    scratch,
                    policy.deadline,
                    policy.cancelled.clone(),
                ),
            };
            let named = read(&fixture.directory.0).unwrap();
            let held = read(&held_path).unwrap();
            assert_eq!(named.tracks(), held.tracks());
            assert_eq!(held.tracks().len(), 1);
            assert_eq!(held.persisted_media_anchor(), 12);
            assert_eq!(
                held.tracks()[0].metadata.title,
                if kind == 1 {
                    "Held WAL-only title"
                } else {
                    "Synthetic Title"
                }
            );
            for catalog in [&named, &held] {
                assert_eq!(catalog.source_hashes().database, db_before.0);
                assert_eq!(catalog.source_hashes().database_bytes, db_before.1);
                assert_eq!(
                    catalog.source_hashes().wal,
                    wal_before.map(|(hash, _)| hash)
                );
                assert_eq!(
                    catalog.source_hashes().wal_bytes,
                    wal_before.map(|(_, bytes)| bytes)
                );
            }
            assert_eq!(
                hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
                db_before
            );
            assert_eq!(
                supplied_wal.map(|p| hash_file(p, MAX_WAL_BYTES).unwrap()),
                wal_before
            );
            if supplied_wal.is_none() {
                assert!(!wal.exists());
            }
            no_scratch_entries(&fixture.directory.0);
        }
    }
}

#[test]
fn renamed_parent_or_child_refuses_and_cleans_only_owned_files() {
    for rename_parent in [false, true] {
        let fixture = Fixture::new(false);
        let input_before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
        let parent = fixture.directory.0.join("parent");
        let moved = fixture.directory.0.join("moved");
        fs::create_dir(&parent).unwrap();
        let held_parent = File::open(&parent).unwrap();
        let locator = PathBuf::from(format!("/proc/self/fd/{}", held_parent.as_raw_fd()));
        let mut owned_path = PathBuf::new();
        let mut replacement = PathBuf::new();
        let result = with_scratch(&locator, None, |scratch| {
            let hashes = copy_fixture(scratch, &fixture, false)?;
            assert_eq!(parse_copy(scratch, hashes)?.tracks().len(), 1);
            owned_path = scratch.path().to_owned();
            if rename_parent {
                fs::rename(&parent, &moved)?;
                fs::create_dir(&parent)?;
                replacement = parent.join(owned_path.file_name().unwrap());
                fs::create_dir(&replacement)?;
            } else {
                fs::rename(&owned_path, &moved)?;
                replacement = owned_path.clone();
                fs::create_dir(&replacement)?;
            }
            fs::write(replacement.join(DB), b"foreign replacement")?;
            Ok(())
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("catalog scratch"));
        assert_eq!(
            fs::read(replacement.join(DB)).unwrap(),
            b"foreign replacement"
        );
        if rename_parent {
            assert!(!moved.join(owned_path.file_name().unwrap()).exists());
        } else {
            assert!(fs::read_dir(&moved).unwrap().next().is_none());
        }
        assert_eq!(
            hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
            input_before
        );
        fs::remove_file(replacement.join(DB)).unwrap();
        fs::remove_dir(&replacement).unwrap();
        fs::remove_dir(&moved).unwrap();
        fs::remove_dir(&parent).unwrap();
    }
}

#[test]
fn replaced_or_symlinked_sqlite_files_refuse_and_survive_cleanup() {
    for leaf in [DB, WAL, SHM] {
        for use_symlink in [false, true] {
            let fixture = Fixture::new(true);
            let db_before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
            let wal_source = fixture.database.with_file_name("fixture.db-wal");
            let wal_before = hash_file(&wal_source, MAX_WAL_BYTES).unwrap();
            let foreign = fixture.directory.0.join("foreign");
            fs::write(&foreign, b"foreign sidecar").unwrap();
            let mut output = PathBuf::new();
            let result = with_scratch(&fixture.directory.0, None, |scratch| {
                let hashes = copy_fixture(scratch, &fixture, true)?;
                assert_eq!(parse_copy(scratch, hashes)?.tracks().len(), 1);
                output = scratch.path().to_owned();
                let path = output.join(leaf);
                // SQLite is allowed to have unlinked its own SHM on close.
                if path.exists() {
                    fs::remove_file(&path)?;
                }
                if use_symlink {
                    symlink(&foreign, &path)?;
                } else {
                    fs::write(&path, b"replacement leaf")?;
                }
                Ok(())
            });
            assert!(format!("{:#}", result.unwrap_err()).contains("catalog scratch"));
            assert_eq!(fs::read(&foreign).unwrap(), b"foreign sidecar");
            assert_eq!(
                fs::read(output.join(leaf)).unwrap(),
                if use_symlink {
                    &b"foreign sidecar"[..]
                } else {
                    &b"replacement leaf"[..]
                }
            );
            assert_eq!(
                fs::symlink_metadata(output.join(leaf))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                use_symlink
            );
            assert_eq!(
                hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
                db_before
            );
            assert_eq!(hash_file(&wal_source, MAX_WAL_BYTES).unwrap(), wal_before);
            fs::remove_file(output.join(leaf)).unwrap();
            fs::remove_dir(output).unwrap();
            fs::remove_file(foreign).unwrap();
        }
    }
}

#[test]
fn parser_failure_still_checks_custody_and_preserves_original_error() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch("DROP TABLE item_stats;")
        .unwrap();
    let before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
    let mut output = PathBuf::new();
    let mut parser_error = String::new();
    let result = with_scratch(&fixture.directory.0, None, |scratch| -> Result<()> {
        let hashes = copy_fixture(scratch, &fixture, false)?;
        let error = match parse_copy(scratch, hashes) {
            Err(error) => error,
            Ok(_) => panic!("invalid catalogue unexpectedly parsed"),
        };
        assert!(matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::QueryReturnedNoRows)
        ));
        parser_error = format!("{error:#}");
        output = scratch.path().to_owned();
        fs::remove_file(output.join(DB))?;
        fs::write(output.join(DB), b"replacement after real parser failure")?;
        Err(error)
    });
    let text = format!("{:#}", result.unwrap_err());
    assert!(
        text.contains("catalog scratch final custody check"),
        "{text}"
    );
    assert!(
        text.contains(&parser_error),
        "original parser cause lost: {text}"
    );
    assert!(
        parser_error.contains("required catalog table absent"),
        "unexpected original parser error: {parser_error}"
    );
    assert_eq!(
        fs::read(output.join(DB)).unwrap(),
        b"replacement after real parser failure"
    );
    assert_eq!(
        hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
        before
    );
    fs::remove_file(output.join(DB)).unwrap();
    fs::remove_dir(output).unwrap();
}

#[test]
fn in_place_copy_mutation_is_refused_after_real_sql() {
    let fixture = Fixture::new(false);
    let before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
    let result = with_scratch(&fixture.directory.0, None, |scratch| {
        let hashes = copy_fixture(scratch, &fixture, false)?;
        assert_eq!(parse_copy(scratch, hashes)?.tracks().len(), 1);
        let copy = &scratch.copies[0];
        let mut byte = [0];
        copy.file.read_exact_at(&mut byte, 100)?;
        byte[0] ^= 1;
        copy.file.write_all_at(&byte, 100)?;
        Ok(())
    });
    assert!(format!("{:#}", result.unwrap_err()).contains("catalog scratch final custody check"));
    assert_eq!(
        hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
        before
    );
    no_scratch_entries(&fixture.directory.0);
}

#[test]
fn late_original_control_cancellation_refuses_success_and_cleans_owned_copies() {
    let fixture = Fixture::new(false);
    let before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
    let policy = control();
    let result = with_scratch(&fixture.directory.0, Some(&policy), |scratch| {
        scratch.copy(&fixture.database, DB, MAX_DATABASE_BYTES, Some(&policy))?;
        policy.cancelled.store(true, Ordering::Relaxed);
        Ok(())
    });
    assert!(format!("{:#}", result.unwrap_err()).contains("cancel"));
    assert_eq!(
        hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
        before
    );
    no_scratch_entries(&fixture.directory.0);
}

#[test]
fn mutable_sidecar_growth_cannot_exceed_owned_bounds() {
    for leaf in [WAL, SHM] {
        let mut fixture = Fixture::new(true);
        let supplied_wal = leaf == SHM;
        if !supplied_wal {
            checkpoint_without_wal(&mut fixture);
        }
        let db_before = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
        let wal = fixture.database.with_file_name("fixture.db-wal");
        let wal_before = supplied_wal.then(|| hash_file(&wal, MAX_WAL_BYTES).unwrap());
        let result = with_scratch(&fixture.directory.0, None, |scratch| {
            copy_fixture(scratch, &fixture, supplied_wal)?;
            let copy = scratch
                .copies
                .iter()
                .find(|copy| copy.name.to_bytes() == leaf.as_bytes())
                .unwrap();
            let cap = copy.mutable_cap.unwrap();
            assert!(cap <= 1024 * 1024, "unexpected synthetic sidecar bound");
            if leaf == WAL {
                assert_eq!(cap, 0);
            }
            copy.file.set_len(cap + 1)?;
            Ok(())
        });
        assert!(format!("{:#}", result.unwrap_err()).contains("sidecar exceeds bound"));
        assert_eq!(
            hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
            db_before
        );
        assert_eq!(
            supplied_wal.then(|| hash_file(&wal, MAX_WAL_BYTES).unwrap()),
            wal_before
        );
        if !supplied_wal {
            assert!(!wal.exists());
        }
        no_scratch_entries(&fixture.directory.0);
    }
}
