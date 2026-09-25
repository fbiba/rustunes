use super::*;
use crate::protocol::transfer::CaptureControl;
use std::io::Write;
use std::sync::{Arc, atomic::AtomicBool};
use tokio::time::Instant;

fn control() -> CaptureControl {
    CaptureControl {
        deadline: Instant::now() + std::time::Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
fn usage(output: &OutputAllowance, bytes: u64, files: u64) {
    assert_eq!(
        output.usage().unwrap(),
        OutputUsage {
            charged_bytes: bytes,
            charged_files: files,
            reserved_bytes: 0
        }
    );
}

#[test]
fn shared_output_charges_json_and_raw_across_clones_without_refunds() {
    let dir = directory();
    let output = OutputAllowance::new(7, 3);
    output.write(&dir.join("first"), b"abc").unwrap();
    output.clone().write_json(&dir.join("json"), &"\n").unwrap();
    assert_eq!(fs::read(dir.join("json")).unwrap(), b"\"\\n\"");
    assert!(output.write(&dir.join("exhausted"), b"x").is_err());
    usage(&output, 7, 3);
    assert!(fs::read(dir.join("exhausted")).unwrap().is_empty());
    assert!(output.write(&dir.join("fourth"), b"").is_err());
    assert!(!dir.join("fourth").exists());
    assert_eq!(
        fs::metadata(dir.join("first"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn json_and_xml_expansion_stop_at_shared_byte_allowance_and_keep_prefixes() {
    for xml in [false, true] {
        let dir = directory();
        let output = OutputAllowance::new(512, 2);
        output.write(&dir.join("earlier"), &[9; 64]).unwrap();
        let path = dir.join("expanded");
        let result = if xml {
            output.write_xml(&path, &plist::Value::String("<&>".repeat(2048)))
        } else {
            output.write_json(&path, &"\u{0001}".repeat(2048))
        };
        assert!(result.is_err());
        let partial = fs::read(&path).unwrap();
        assert!(!partial.is_empty());
        assert!(partial.len() <= 448);
        assert_eq!(
            output.usage().unwrap().charged_bytes,
            64 + partial.len() as u64
        );
        assert_eq!(output.usage().unwrap().charged_files, 2);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn failed_creations_consume_file_attempts_without_overwriting_or_following_links() {
    let dir = directory();
    let path = dir.join("retained");
    fs::write(&path, b"prior bytes").unwrap();
    symlink(&path, dir.join("link")).unwrap();
    let output = OutputAllowance::new(100, 2);
    assert!(output.write(&path, b"new").is_err());
    assert!(output.write(&dir.join("link"), b"new").is_err());
    assert!(output.write(&dir.join("next"), b"new").is_err());
    usage(&output, 0, 2);
    assert_eq!(fs::read(path).unwrap(), b"prior bytes");
    assert!(!dir.join("next").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn controlled_output_keeps_prior_bytes_and_charge_after_cancellation_or_deadline() {
    for expired in [false, true] {
        let dir = directory();
        let control = control();
        let output = OutputAllowance::new(100, 2);
        let path = dir.join("partial");
        let mut file = output.create_controlled(&path, &control).unwrap();
        file.write_all(b"before").unwrap();
        if expired {
            tokio::time::advance(std::time::Duration::from_secs(6)).await;
        } else {
            control.cancelled.store(true, Ordering::Relaxed);
        }
        assert!(file.write_all(b"after").is_err());
        assert!(file.finish().is_err());
        usage(&output, 6, 1);
        assert_eq!(fs::read(&path).unwrap(), b"before");
        // Emergency retention of already consumed input is still bounded even
        // though capture control can no longer authorize new work.
        output
            .write(&dir.join("received-partial"), b"wire")
            .unwrap();
        usage(&output, 10, 2);
        control.cancelled.store(true, Ordering::Relaxed);
        assert!(
            output
                .create_controlled(&dir.join("cancelled"), &control)
                .is_err()
        );
        usage(&output, 10, 2);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn actual_file_write_failure_retains_full_charge_and_poisoned_sink() {
    let dir = directory();
    let path = dir.join("failed");
    let output = OutputAllowance::new(5, 2);
    let mut file = output.create(&path).unwrap();
    // Reproduce a real write error using the same private file opened read-only;
    // no special device, disk exhaustion or filesystem permission assumption.
    file.file = fs::File::open(&path).unwrap();
    assert!(file.write_all(b"abc").is_err());
    usage(&output, 3, 1);
    assert!(file.write_all(b"x").is_err());
    assert!(file.finish().is_err());
    output.write(&dir.join("remaining"), b"de").unwrap();
    usage(&output, 5, 2);
    assert!(fs::read(path).unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn serializer_error_retains_generated_bytes_and_file_charge() {
    struct Failure;
    impl serde::Serialize for Failure {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(None)?;
            sequence.serialize_element("retained")?;
            Err(serde::ser::Error::custom("stop after partial output"))
        }
    }
    let dir = directory();
    let output = OutputAllowance::new(100, 1);
    assert!(output.write_json(&dir.join("partial"), &Failure).is_err());
    let bytes = fs::read(dir.join("partial")).unwrap();
    assert_eq!(bytes, b"[\"retained\"");
    usage(&output, bytes.len() as u64, 1);
    assert!(output.write_json(&dir.join("retry"), &true).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_private_writers_cannot_double_spend_shared_allowance() {
    let dir = directory();
    let output = OutputAllowance::new(7, 2);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let joins: Vec<_> = (0..2)
        .map(|i| {
            let output = output.clone();
            let barrier = barrier.clone();
            let path = dir.join(i.to_string());
            std::thread::spawn(move || {
                let mut file = output.create(&path).unwrap();
                barrier.wait();
                file.write_all(b"four")
                    .and_then(|_| file.finish().map_err(std::io::Error::other))
                    .is_ok()
            })
        })
        .collect();
    let passed = joins
        .into_iter()
        .map(|join| usize::from(join.join().unwrap()))
        .sum::<usize>();
    assert_eq!(passed, 1);
    usage(&output, 4, 2);
    let actual: u64 = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert_eq!(actual, 4);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn input_reservation_protects_partial_bytes_from_other_writers_and_releases_only_unused_capacity() {
    let dir = directory();
    let output = OutputAllowance::new(12, 3);
    let reservation = output.reserve_file(4, 8).unwrap();
    assert_eq!(reservation.capacity(), 8);
    assert_eq!(output.usage().unwrap().reserved_bytes, 8);
    output.write(&dir.join("concurrent"), b"four").unwrap();
    assert!(output.write(&dir.join("blocked"), b"x").is_err());
    reservation.write(&dir.join("partial"), b"sixsix").unwrap();
    usage(&output, 10, 3);
    assert_eq!(fs::read(dir.join("partial")).unwrap(), b"sixsix");
    assert!(output.reserve_file(1, 2).is_err());
    fs::remove_dir_all(dir).unwrap();
}
