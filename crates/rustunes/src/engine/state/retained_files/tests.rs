//! Retained-file policy checks.
use crate::engine::state::retained_files as retained;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct PrivateRoot(PathBuf);
impl PrivateRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "rustunes-retained-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
}
impl Drop for PrivateRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn retained_files_preserve_private_bytes_bounds_and_existing_hardlink_policy() {
    let root = PrivateRoot::new();
    let directory = root.0.join("evidence");
    retained::make_directory(&directory).unwrap();
    let source = directory.join("source");
    let bytes = b"retained evidence bytes";
    retained::write(&source, bytes).unwrap();
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(retained::read(&source, bytes.len() as u64).unwrap(), bytes);
    assert_eq!(
        retained::file_hash(&source, bytes.len() as u64).unwrap(),
        (bytes.len() as u64, retained::sha(bytes))
    );
    assert!(retained::read(&source, (bytes.len() - 1) as u64).is_err());
    assert!(retained::file_hash(&source, (bytes.len() - 1) as u64).is_err());
    assert!(retained::write(&source, b"replacement").is_err());
    assert_eq!(fs::read(&source).unwrap(), bytes);

    // The retained reader accepts a hardlinked file. Callers that need a single
    // link check `nlink` themselves, as `apply::finish_alac_retention` does.
    let linked = directory.join("linked");
    fs::hard_link(&source, &linked).unwrap();
    assert_eq!(retained::read(&linked, 1024).unwrap(), bytes);
    assert_eq!(
        retained::file_hash(&linked, 1024).unwrap().1,
        retained::sha(bytes)
    );
}

#[test]
fn retained_files_reject_symlinks_and_nonprivate_paths_without_modifying_input() {
    let root = PrivateRoot::new();
    let source = root.0.join("source");
    retained::write(&source, b"original").unwrap();
    let alias = root.0.join("alias");
    symlink(&source, &alias).unwrap();
    assert!(retained::read(&alias, 1024).is_err());
    assert!(retained::file_hash(&alias, 1024).is_err());
    assert!(retained::write(&alias, b"replacement").is_err());
    let directory_alias = root.0.join("directory-alias");
    symlink(&root.0, &directory_alias).unwrap();
    assert!(retained::directory(&directory_alias).is_err());
    assert!(retained::read(&directory_alias.join("source"), 1024).is_err());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(retained::read(&source, 1024).is_err());
    assert!(retained::file_hash(&source, 1024).is_err());
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(retained::directory(&root.0).is_err());
    assert!(retained::write(&root.0.join("forbidden"), b"new").is_err());
    assert!(!root.0.join("forbidden").exists());
    assert_eq!(fs::read(&source).unwrap(), b"original");
}

#[test]
fn retained_hash_control_refuses_before_access_and_preserves_error_contract() {
    let root = PrivateRoot::new();
    let missing = root.0.join("missing");
    let cancelled = Arc::new(AtomicBool::new(true));
    let error = retained::file_hash_with_control(
        &missing,
        1024,
        Instant::now() + Duration::from_secs(30),
        cancelled,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "retained hash cancelled");
    let error = retained::file_hash_with_control(
        &missing,
        1024,
        Instant::now() - Duration::from_secs(1),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "retained hash deadline exceeded");
    assert!(!missing.exists());
}

#[test]
fn retained_hash_controlled_preserves_private_guards_and_observes_cancel() {
    let private = PrivateRoot::new();
    let root = private.0.join("evidence");
    retained::make_directory(&root).unwrap();
    let source = root.join("source");
    let bytes = b"bounded retained source";
    retained::write(&source, bytes).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    let cancelled = Arc::new(AtomicBool::new(false));
    assert_eq!(
        retained::file_hash_with_control(&source, 1024, deadline, cancelled.clone()).unwrap(),
        retained::file_hash(&source, 1024).unwrap()
    );
    cancelled.store(true, Ordering::Relaxed);
    assert!(retained::file_hash_with_control(&source, 1024, deadline, cancelled.clone()).is_err());
    cancelled.store(false, Ordering::Relaxed);
    assert!(
        retained::file_hash_with_control(
            &source,
            1024,
            Instant::now() - std::time::Duration::from_secs(1),
            cancelled.clone()
        )
        .is_err()
    );
    assert!(
        retained::file_hash_with_control(
            &source,
            (bytes.len() - 1) as u64,
            deadline,
            cancelled.clone()
        )
        .is_err()
    );
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&source, &alias).unwrap();
    assert!(retained::file_hash_with_control(&alias, 1024, deadline, cancelled.clone()).is_err());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(retained::file_hash_with_control(&source, 1024, deadline, cancelled).is_err());
    assert_eq!(fs::read(&source).unwrap(), bytes);
}
