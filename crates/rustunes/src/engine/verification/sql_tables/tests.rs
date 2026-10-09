use super::*;
use reader::{Budget, Cell, Rows};
use sha2::{Digest, Sha256};

use music_protocol::test_support::catalogue::NativeFixture;
use std::{fs, ops::Deref};
/// The shared native-schema pair; its directory goes when it is dropped.
pub(crate) struct Fixture(NativeFixture);
impl Deref for Fixture {
    type Target = NativeFixture;
    fn deref(&self) -> &NativeFixture {
        &self.0
    }
}
impl Fixture {
    pub(crate) fn new() -> Self {
        Self(NativeFixture::new())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
#[test]
fn typed_multisets_preserve_real_bits_and_multiplicity() {
    let a = vec![
        Cell::Real(0),
        Cell::Null,
        Cell::Integer(1),
        Cell::Text("1".into()),
        Cell::Blob(vec![49]),
    ];
    let mut b = a.clone();
    b[0] = Cell::Real((-0.0f64).to_bits());
    assert_ne!(a, b);
    let mut x = Rows::new();
    x.insert(a.clone(), 2);
    let mut y = x.clone();
    y.insert(a, 1);
    assert_ne!(x, y);
}
#[cfg(unix)]
#[test]
fn reader_rejects_symlink_fifo_and_oversize_without_blocking() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    let f = Fixture::new();
    let link = f.dir.join("link");
    symlink(&f.before, &link).unwrap();
    assert!(reader::read(&link, reader::DB_CAP, &Budget::new()).is_err());
    let fifo = f.dir.join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(reader::read(&fifo, reader::DB_CAP, &Budget::new()).is_err());
    assert!(reader::read(&f.before, 1, &Budget::new()).is_err());
}

#[cfg(unix)]
#[test]
fn held_materialization_preserves_full_native_schema() {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };
    // Real host SQLite with the repository's full schema, not receiver qualification.
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    let before = fs::read(&fixture.before).unwrap();
    let capture = Capture {
        database: &fixture.before,
        database_sha256: Sha256::digest(&before).into(),
        wal: None,
    };
    let mut budget = Budget::new_cancellable_until(
        Instant::now() + Duration::from_secs(60),
        Arc::new(AtomicBool::new(false)),
    );
    reader::with_materialized_controlled(
        capture,
        &fixture.dir.join("custody-full"),
        &mut budget,
        |view| {
            assert_eq!(view.snapshot().tables.len(), 45);
            assert_eq!(view.snapshot().version, 2_400_060);
            assert!(view.snapshot().layout_qualified().unwrap());
            view.recheck()
        },
    )
    .unwrap();
    assert_eq!(fs::read(&fixture.before).unwrap(), before);
}
