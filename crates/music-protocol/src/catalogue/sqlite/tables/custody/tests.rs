use super::super::{Cell, configure_memory_temp, materialize};
use super::*;
use rusqlite::Connection;
use std::{
    cell::Cell as Flag,
    fs::Permissions,
    io::Read,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Case {
    directory: PathBuf,
    database: PathBuf,
    wal: Option<PathBuf>,
}
impl Case {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sql-custody-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let database = directory.join("source.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE facts(n INTEGER,r REAL,t TEXT,b BLOB,z);
            INSERT INTO facts VALUES(7,2.5,'label',X'0011',NULL),(7,2.5,'label',X'0011',NULL);
            PRAGMA user_version=2400060;",
            )
            .unwrap();
        drop(connection);
        Self {
            directory,
            database,
            wal: None,
        }
    }
    fn with_wal() -> Self {
        let mut case = Self::new();
        let connection = Connection::open(&case.database).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; UPDATE facts SET n=9;")
            .unwrap();
        let captured = case.directory.join("captured.sqlite");
        let wal = case.directory.join("captured.sqlite-wal");
        // Keep the source peer descriptors until SQLite closes: closing a
        // same-inode peer while its connection lives can release POSIX locks.
        let mut source_db = File::open(&case.database).unwrap();
        let mut source_wal = File::open(case.database.with_file_name("source.sqlite-wal")).unwrap();
        let mut captured_db = File::create(&captured).unwrap();
        let mut captured_wal = File::create(&wal).unwrap();
        std::io::copy(&mut source_db, &mut captured_db).unwrap();
        std::io::copy(&mut source_wal, &mut captured_wal).unwrap();
        captured_db.sync_all().unwrap();
        captured_wal.sync_all().unwrap();
        assert!(captured_wal.metadata().unwrap().len() > 32);
        drop(connection);
        drop(source_db);
        drop(source_wal);
        case.database = captured;
        case.wal = Some(wal);
        case
    }
    fn capture(&self) -> Capture<'_> {
        Capture {
            database: &self.database,
            database_sha256: Sha256::digest(fs::read(&self.database).unwrap()).into(),
            wal: self.wal.as_ref().map(|path| {
                (
                    path.as_path(),
                    Sha256::digest(fs::read(path).unwrap()).into(),
                )
            }),
        }
    }
    fn bytes(&self) -> (Vec<u8>, Option<Vec<u8>>) {
        (
            fs::read(&self.database).unwrap(),
            self.wal.as_ref().map(|path| fs::read(path).unwrap()),
        )
    }
    fn out(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn controlled() -> (Budget, Arc<AtomicBool>) {
    let flag = Arc::new(AtomicBool::new(false));
    (
        Budget::new_cancellable_until(Instant::now() + Duration::from_secs(60), flag.clone()),
        flag,
    )
}
fn replace_equal(path: &Path, saved: &Path) {
    fs::rename(path, saved).unwrap();
    fs::copy(saved, path).unwrap();
}

#[test]
fn held_materialization_preserves_full_typed_schema_and_multiplicity() {
    let case = Case::new();
    let before = case.bytes();
    let (mut budget, _) = controlled();
    let facts =
        with_materialized_controlled(case.capture(), &case.out("typed"), &mut budget, |view| {
            Ok(view.snapshot().clone())
        })
        .unwrap();
    let expected = vec![
        Cell::Integer(7),
        Cell::Real(2.5f64.to_bits()),
        Cell::Text("label".into()),
        Cell::Blob(vec![0, 17]),
        Cell::Null,
    ];
    assert_eq!(facts.table("facts").unwrap().rows.get(&expected), Some(&2));
    let mut legacy = Budget::new();
    let old = materialize(case.capture(), &case.out("legacy"), &mut legacy).unwrap();
    assert_eq!(old.tables, facts.tables);
    assert_eq!(old.schema, facts.schema);
    assert_eq!(case.bytes(), before);
    assert_eq!(fs::read_dir(case.out("typed")).unwrap().count(), 1);
}

#[test]
fn held_materialization_borrows_the_original_budget() {
    let case = Case::new();
    let (mut budget, flag) = controlled();
    let deadline = budget.deadline;
    with_materialized_controlled(case.capture(), &case.out("budget"), &mut budget, |view| {
        assert_eq!(view.budget().deadline, deadline);
        assert!(Arc::ptr_eq(
            view.budget().cancelled.as_ref().unwrap(),
            &flag
        ));
        view.recheck()
    })
    .unwrap();
}

#[test]
fn held_materialization_distinguishes_absent_empty_and_committed_wal() {
    for mode in 0..3 {
        let mut case = if mode == 2 {
            Case::with_wal()
        } else {
            Case::new()
        };
        if mode == 1 {
            let wal = case.directory.join("source.sqlite-wal");
            fs::write(&wal, []).unwrap();
            case.wal = Some(wal);
        }
        let before = case.bytes();
        let output = case.out("proof");
        let (mut budget, _) = controlled();
        let snapshot = with_materialized_controlled(case.capture(), &output, &mut budget, |view| {
            view.recheck()?;
            Ok(view.snapshot().clone())
        })
        .unwrap();
        assert_eq!(
            snapshot.hashes.wal_bytes,
            case.wal.as_ref().map(|p| fs::metadata(p).unwrap().len())
        );
        assert_eq!(snapshot.hashes.wal_sha256.is_some(), mode != 0);
        assert_eq!(
            snapshot.table("facts").unwrap().rows.keys().next().unwrap()[0],
            Cell::Integer(if mode == 2 { 9 } else { 7 })
        );
        assert_eq!(fs::read(output.join(DB)).unwrap(), before.0);
        if let Some(wal) = &before.1 {
            assert_eq!(fs::read(output.join(WAL)).unwrap(), *wal);
        }
        assert!(!output.join(SHM).exists());
        assert_eq!(
            fs::read_dir(&output).unwrap().count(),
            if mode == 0 { 1 } else { 2 }
        );
        assert_eq!(case.bytes(), before);
    }
}

#[test]
fn held_creation_identity_refuses_equal_bytes_and_mutated_payloads() {
    for (wal, equal) in [(false, true), (true, true), (false, false), (true, false)] {
        let case = if wal { Case::with_wal() } else { Case::new() };
        let before = case.bytes();
        let output = case.out("proof");
        let selected = if wal { WAL } else { DB };
        let saved = case.out("displaced");
        let called = Flag::new(false);
        let (mut budget, _) = controlled();
        let error = with_checkpoints(
            case.capture(),
            &output,
            &mut budget,
            |_| {
                if equal {
                    replace_equal(&output.join(selected), &saved);
                    assert_eq!(
                        fs::read(&saved).unwrap(),
                        fs::read(output.join(selected)).unwrap()
                    );
                } else {
                    let file = File::options()
                        .write(true)
                        .open(output.join(selected))
                        .unwrap();
                    file.write_at(&[0x7f], 0).unwrap();
                    file.sync_all().unwrap();
                }
                Ok(())
            },
            |_, _, _| Ok(()),
            |_| {
                called.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(!called.get(), "creation replacement reached visitor");
        assert!(format!("{error:#}").contains("SQL leaf"), "{error:#}");
        assert_eq!(case.bytes(), before);
    }
}

#[test]
fn held_namespace_refuses_aliases_extra_entries_and_substituted_roots() {
    for kind in [
        "extra", "missing", "symlink", "alias", "mode", "root", "ancestor",
    ] {
        let case = Case::new();
        let parent = case.out("parent");
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        let output = parent.join("proof");
        let called = Flag::new(false);
        let (mut budget, _) = controlled();
        let error = with_checkpoints(
            case.capture(),
            &output,
            &mut budget,
            |_| {
                match kind {
                    "extra" => {
                        fs::write(output.join("foreign"), b"not owned").unwrap();
                    }
                    "missing" => {
                        fs::remove_file(output.join(DB)).unwrap();
                    }
                    "symlink" => {
                        fs::remove_file(output.join(DB)).unwrap();
                        std::os::unix::fs::symlink(&case.database, output.join(DB)).unwrap();
                    }
                    "alias" => {
                        fs::hard_link(output.join(DB), output.join("alias")).unwrap();
                    }
                    "mode" => {
                        fs::set_permissions(output.join(DB), Permissions::from_mode(0o644))
                            .unwrap();
                    }
                    "root" => {
                        fs::rename(&output, case.out("detached")).unwrap();
                        fs::DirBuilder::new().mode(0o700).create(&output).unwrap();
                    }
                    "ancestor" => {
                        fs::rename(&parent, case.out("detached-parent")).unwrap();
                        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
                    }
                    _ => unreachable!(),
                }
                Ok(())
            },
            |_, _, _| Ok(()),
            |_| {
                called.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(!called.get(), "{kind} reached visitor");
        assert!(format!("{error:#}").contains("SQL"), "{kind}: {error:#}");
    }
}

// A fresh child uses only async-signal-safe libc calls; it never executes SQLite.
// F_GETLK from another process can observe locks belonging to the parent. Closing
// a peer same-inode fd in the parent would release its SHM DMS lock.
fn parent_holds_dms(fd: i32) -> bool {
    let mut pipe = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let parent_pid = unsafe { libc::getpid() };
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        panic!("local lock-control fork failed");
    }
    if pid == 0 {
        unsafe {
            libc::close(pipe[0]);
            let mut lock: libc::flock = std::mem::zeroed();
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = 128; // bundled Unix WAL-index DMS byte
            lock.l_len = 1;
            let ok = libc::fcntl(fd, libc::F_GETLK, &mut lock) == 0
                && lock.l_type != libc::F_UNLCK as libc::c_short
                && lock.l_pid == parent_pid;
            let result = [u8::from(ok)];
            libc::write(pipe[1], result.as_ptr().cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[1]);
    }
    let mut input = unsafe { File::from_raw_fd(pipe[0]) };
    let mut result = [0];
    input.read_exact(&mut result).unwrap();
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            break;
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINTR)
        );
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    result[0] == 1
}

#[test]
fn held_wal_scratch_preserves_locks_and_removes_only_owned_shm() {
    let case = Case::with_wal();
    let before = case.bytes();
    let output = case.out("proof");
    let (mut budget, _) = controlled();
    let inode = Flag::new(None);
    with_checkpoints(
        case.capture(),
        &output,
        &mut budget,
        |created| {
            let shm = created.shm.as_ref().unwrap();
            inode.set(Some((shm.metadata.dev(), shm.metadata.ino())));
            Ok(())
        },
        |connection, created, original_budget| {
            assert_eq!(
                connection
                    .query_row("SELECT n FROM facts LIMIT 1", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                9
            );
            let shm = created.shm.as_ref().unwrap();
            assert!(shm.file.metadata().unwrap().len() > 0);
            assert_eq!(
                inode.get(),
                Some((
                    shm.file.metadata().unwrap().dev(),
                    shm.file.metadata().unwrap().ino()
                ))
            );
            assert!(
                parent_holds_dms(shm.file.as_raw_fd()),
                "SQLite did not retain real SHM DMS lock"
            );
            // The actual production guard must preserve the active lock.
            created.check_live(original_budget)?;
            assert!(
                parent_holds_dms(shm.file.as_raw_fd()),
                "custody guard released SQLite's real SHM DMS lock"
            );
            Ok(())
        },
        |view| {
            view.recheck()?;
            Ok(())
        },
    )
    .unwrap();
    assert!(!output.join(SHM).exists());
    assert_eq!(case.bytes(), before);
    let (mut budget, _) = controlled();
    let connection = Connection::open_in_memory().unwrap();
    configure_memory_temp(&connection, &budget).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA temp_store", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(connection);
    let foreign = case.out("foreign-shm");
    let saved = case.out("old-shm");
    let error = with_checkpoints(
        case.capture(),
        &foreign,
        &mut budget,
        |_| {
            replace_equal(&foreign.join(SHM), &saved);
            Ok(())
        },
        |_, _, _| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("SHM"), "{error:#}");
    assert!(foreign.join(SHM).exists(), "foreign scratch was deleted");
}

#[test]
fn held_callback_rechecks_custody_and_combines_visitor_errors() {
    for refusal in [false, true] {
        let case = Case::new();
        let output = case.out("proof");
        let saved = case.out("old-copy");
        let (mut budget, _) = controlled();
        let error = with_materialized_controlled(case.capture(), &output, &mut budget, |view| {
            view.recheck()?;
            replace_equal(&output.join(DB), &saved);
            if refusal {
                bail!("visitor refusal marker");
            }
            Ok(())
        })
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("SQL leaf"), "{message}");
        if refusal {
            assert!(
                message.contains("visitor refusal marker")
                    && message.contains("final check also failed"),
                "{message}"
            );
        }
    }
    // Original input errors must not prevent the independent output guard check.
    let case = Case::new();
    let output = case.out("both");
    let (mut budget, _) = controlled();
    let error = with_materialized_controlled::<()>(case.capture(), &output, &mut budget, |_| {
        fs::write(&case.database, b"changed source").unwrap();
        fs::remove_file(output.join(DB)).unwrap();
        bail!("visitor multi-failure marker")
    })
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("visitor multi-failure marker")
            && message.contains("source database changed")
            && message.contains("creation-held SQL output final check"),
        "{message}"
    );
    // A real preanalysis live guard failure or a real analyzed-Connection
    // refusal must not discard accepted input facts because Snapshot is absent.
    for stage in ["preanalysis", "analysis-source", "analysis-source-output"] {
        let case = if stage == "preanalysis" {
            Case::new()
        } else {
            Case::with_wal()
        };
        let before = case.bytes();
        let output = case.out("previsitor-failure");
        let reached = Flag::new(false);
        let visited = Flag::new(false);
        let (mut budget, flag) = controlled();
        let original_deadline = budget.deadline;
        let error = with_checkpoints::<()>(
            case.capture(),
            &output,
            &mut budget,
            |_| {
                if stage == "preanalysis" {
                    reached.set(true);
                    fs::write(&case.database, b"changed accepted input").unwrap();
                    // No SQLite connection exists yet, so this mutation's peer FD
                    // closes before analysis could begin.
                    let file = File::options().write(true).open(output.join(DB)).unwrap();
                    file.write_at(&[0x7f], 0).unwrap();
                    file.sync_all().unwrap();
                }
                Ok(())
            },
            |connection, _, original_budget| {
                assert_ne!(stage, "preanalysis", "failed live guard reached analysis");
                reached.set(true);
                assert_eq!(original_budget.deadline, original_deadline);
                assert!(Arc::ptr_eq(
                    original_budget.cancelled.as_ref().unwrap(),
                    &flag
                ));
                assert_eq!(
                    connection
                        .query_row("SELECT n FROM facts LIMIT 1", [], |row| row
                            .get::<_, i64>(0))
                        .unwrap(),
                    9
                );
                // The original capture is a different inode from SQLite's copies.
                fs::write(&case.database, b"changed accepted input").unwrap();
                if stage == "analysis-source-output" {
                    // Unlink does not close a peer SQLite FD. The inner Connection
                    // still drops before the actual final output/source checks.
                    fs::remove_file(output.join(DB)).unwrap();
                }
                bail!("analysis refusal marker: {stage}")
            },
            |_| {
                visited.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            reached.get() && !visited.get(),
            "{stage}: checkpoint/visitor mismatch"
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("accepted SQL input final check")
                && message.contains("source database changed"),
            "{stage}: {message}"
        );
        if stage == "preanalysis" {
            assert!(message.contains("SQL leaf"), "{stage}: {message}");
        } else {
            assert!(
                message.contains("analysis refusal marker"),
                "{stage}: {message}"
            );
        }
        if stage != "analysis-source" {
            assert!(
                message.contains("creation-held SQL output final check"),
                "{stage}: {message}"
            );
        } else {
            // Source refusal does not suppress the valid bounded scratch close.
            assert!(!output.join(SHM).exists());
            assert_eq!(fs::read(output.join(DB)).unwrap(), before.0);
            assert_eq!(fs::read(output.join(WAL)).unwrap(), before.1.unwrap());
        }
    }
}

#[test]
fn held_original_budget_refuses_cancelled_and_expired_stages() {
    let case = Case::new();
    for expired in [false, true] {
        let output = case.out(if expired { "expired" } else { "cancelled" });
        let flag = Arc::new(AtomicBool::new(!expired));
        let deadline = if expired {
            Instant::now() - Duration::from_secs(1)
        } else {
            Instant::now() + Duration::from_secs(60)
        };
        let mut budget = Budget::new_cancellable_until(deadline, flag);
        assert!(
            with_materialized_controlled(case.capture(), &output, &mut budget, |_| Ok(())).is_err()
        );
        assert!(!output.exists(), "prewrite control refusal created output");
    }
    let output = case.out("creation-cancel");
    let (mut budget, flag) = controlled();
    let error = with_checkpoints(
        case.capture(),
        &output,
        &mut budget,
        |_| {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        },
        |_, _, _| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    assert_eq!(
        fs::read(output.join(DB)).unwrap(),
        fs::read(&case.database).unwrap()
    );
    let output = case.out("visitor-cancel");
    let (mut budget, flag) = controlled();
    let error = with_materialized_controlled(case.capture(), &output, &mut budget, |_| {
        flag.store(true, Ordering::Relaxed);
        Ok(())
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    let case = Case::with_wal();
    let output = case.out("live-cancel-errors");
    let (mut budget, flag) = controlled();
    let original_deadline = budget.deadline;
    let reached = Flag::new(false);
    let visited = Flag::new(false);
    let error = with_checkpoints::<()>(
        case.capture(),
        &output,
        &mut budget,
        |_| Ok(()),
        |connection, _, original_budget| {
            reached.set(true);
            assert_eq!(original_budget.deadline, original_deadline);
            assert!(Arc::ptr_eq(
                original_budget.cancelled.as_ref().unwrap(),
                &flag
            ));
            assert_eq!(
                connection
                    .query_row("SELECT n FROM facts LIMIT 1", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                9
            );
            fs::write(&case.database, b"changed accepted input").unwrap();
            fs::remove_file(output.join(DB)).unwrap();
            flag.store(true, Ordering::Relaxed);
            bail!("live cancel multi-failure marker")
        },
        |_| {
            visited.set(true);
            Ok(())
        },
    )
    .unwrap_err();
    assert!(reached.get() && !visited.get());
    let message = format!("{error:#}");
    // Once cancelled, no unbounded hash/cleanup is allowed. These distinct
    // contexts prove the accepted-input/output/original-control checks refused;
    // they deliberately do not claim the tampered input was then SHA-verified.
    assert!(
        message.contains("live cancel multi-failure marker")
            && message.contains("cancelled")
            && message.contains("accepted SQL input final check")
            && message.contains("creation-held SQL output final check")
            && message.contains("original SQL Budget final check"),
        "{message}"
    );
}

mod owned;
