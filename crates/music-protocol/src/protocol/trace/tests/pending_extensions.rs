use super::*;
use std::io::Write;

#[test]
fn reservation_grows_atomically_without_releasing_protected_header_capacity() {
    let dir = directory();
    let output = OutputAllowance::new(20, 3);
    let mut header = output.reserve_file(4, 4).unwrap();
    output.write(&dir.join("other-phase"), b"eight888").unwrap();
    let before = output.usage().unwrap();
    assert!(header.grow_to(16).is_err());
    assert_eq!(header.capacity(), 4);
    assert_eq!(output.usage().unwrap(), before);
    header.grow_to(12).unwrap();
    assert_eq!(header.capacity(), 12);
    assert_eq!(output.usage().unwrap().reserved_bytes, 12);
    assert!(output.write(&dir.join("cannot-steal"), b"x").is_err());
    header.grow_to(4).unwrap();
    assert_eq!(
        header.capacity(),
        12,
        "a lower request cannot shrink held capacity"
    );
    header
        .write(&dir.join("received"), b"twelve123456")
        .unwrap();
    let usage = output.usage().unwrap();
    assert_eq!(usage.charged_bytes, 20);
    assert_eq!(usage.reserved_bytes, 0);
    assert_eq!(fs::read(dir.join("received")).unwrap(), b"twelve123456");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn progress_sync_preserves_live_append_and_seals_only_on_finish() {
    let dir = directory();
    let output = OutputAllowance::new(100, 1);
    let path = dir.join("progress");
    let mut progress = output.create(&path).unwrap();
    progress.write_all(b"first\n").unwrap();
    progress.sync_all().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first\n");
    progress.write_all(b"second\n").unwrap();
    progress.sync_all().unwrap();
    progress.finish().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first\nsecond\n");
    assert_eq!(output.usage().unwrap().charged_bytes, 13);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o400
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cancelled_progress_sync_poisoning_retains_prior_bytes_and_charges() {
    use crate::protocol::transfer::CaptureControl;
    use std::sync::{Arc, atomic::AtomicBool};
    let dir = directory();
    let output = OutputAllowance::new(100, 1);
    let control = CaptureControl {
        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let path = dir.join("progress");
    let mut progress = output.create_controlled(&path, &control).unwrap();
    progress.write_all(b"first\n").unwrap();
    progress.sync_all().unwrap();
    control.cancelled.store(true, Ordering::Relaxed);
    assert!(progress.sync_all().is_err());
    control.cancelled.store(false, Ordering::Relaxed);
    assert!(progress.write_all(b"second\n").is_err());
    assert!(progress.finish().is_err());
    assert_eq!(fs::read(path).unwrap(), b"first\n");
    assert_eq!(output.usage().unwrap().charged_bytes, 6);
    fs::remove_dir_all(dir).unwrap();
}
