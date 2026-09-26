use super::*;
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "held-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self) -> PathBuf {
        let path = self.0.join("source.m4a");
        fs::write(
            &path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bounded_source_rejects_symlink_replacement_and_same_size_edits() {
    let temp = Temp::new();
    let path = temp.source();
    let fingerprint = crate::media::fingerprint::read(&path).unwrap();
    let source = BoundSource::open(&path, &fingerprint).unwrap();
    fs::rename(&path, temp.0.join("original.m4a")).unwrap();
    fs::write(
        &path,
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/silence-alac.m4a"
        )),
    )
    .unwrap();
    assert!(source.check().is_err());
    let source = BoundSource::open(&path, &fingerprint).unwrap();
    let mut data = fs::read(&path).unwrap();
    data[20] ^= 1;
    fs::write(&path, &data).unwrap();
    assert!(source.check().is_err());
    assert!(BoundSource::open(&path, &fingerprint).is_err());
    let symlink = temp.0.join("link.m4a");
    std::os::unix::fs::symlink(temp.0.join("original.m4a"), &symlink).unwrap();
    assert!(BoundSource::open(&symlink, &fingerprint).is_err());
}

#[test]
fn recovery_source_retention_uses_pinned_handle_and_refuses_later_edits() {
    let temp = Temp::new();
    let path = temp.source();
    let fingerprint = crate::media::fingerprint::read(&path).unwrap();
    let original = fs::read(&path).unwrap();
    let mut bound = BoundSource::open(&path, &fingerprint).unwrap();
    assert_eq!(bound.read_for_retention().unwrap(), original);
    // A repeated read must rewind the pinned handle rather than retain an empty file.
    assert_eq!(bound.read_for_retention().unwrap(), original);
    let mut changed = original.clone();
    *changed.last_mut().unwrap() ^= 1;
    fs::write(&path, changed).unwrap();
    assert!(bound.read_for_retention().is_err());

    fs::write(&path, &original).unwrap();
    let mut bound = BoundSource::open(&path, &fingerprint).unwrap();
    fs::rename(&path, temp.0.join("old-source.m4a")).unwrap();
    fs::write(&path, original).unwrap();
    assert!(bound.read_for_retention().is_err());
}

#[test]
fn source_replaced_between_stat_and_open_cannot_block_on_fifo_or_follow_symlink() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    for fifo in [false, true] {
        let temp = Temp::new();
        let path = temp.source();
        let fingerprint = crate::media::fingerprint::read(&path).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        let retained = temp.0.join("retained.m4a");
        fs::rename(&path, &retained).unwrap();
        if fifo {
            let name = CString::new(path.as_os_str().as_bytes()).unwrap();
            // No writer exists: a blocking open would hang this deterministic
            // regression before the descriptor identity check could run.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        } else {
            std::os::unix::fs::symlink(retained, &path).unwrap();
        }
        assert!(
            BoundSource::open_pinned(
                &path,
                metadata,
                fingerprint.file_bytes,
                fingerprint.full_file_sha256
            )
            .is_err()
        );
    }
}

#[test]
fn source_validation_returns_the_same_rewound_handle() {
    let temp = Temp::new();
    let path = temp.source();
    let fingerprint = crate::media::fingerprint::read(&path).unwrap();
    let mut bound = BoundSource::open(&path, &fingerprint).unwrap();
    let mut header = [0; 8];
    bound.file.read_exact(&mut header).unwrap();
    assert_eq!(
        &header,
        &include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/silence-alac.m4a"
        ))[..8]
    );
    bound.check().unwrap();
    assert_eq!(bound.file.metadata().unwrap().ino(), bound.metadata.ino());
}
