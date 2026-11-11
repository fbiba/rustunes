use super::*;
use crate::engine::state::current::{
    Current, DeviceFile,
    fixtures::{self, Saved, Shape},
    receipt::{self, Form},
};
use anyhow::Result;
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "saved-evidence-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
    /// A capture of `shape` in a directory of its own.
    fn saved(&self, shape: Shape) -> Saved {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        fixtures::write(&self.0.join(format!("saved-{n}")), shape)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load(t: &Temp, saved: &Saved) -> Result<Current> {
    let spec = SavedCurrent {
        directory: saved.directory.clone(),
        receipt_sha256: io::hex(&saved.current.receipt_sha256),
    };
    Ok(current::load_current(&spec, fixtures::DEVICE, &t.0)?.current)
}

/// The loaded Current is the one the capture returned.
fn assert_same(loaded: &Current, captured: &Current) {
    let (a, b) = (
        loaded.catalog.source_hashes(),
        captured.catalog.source_hashes(),
    );
    assert!(
        loaded.device == captured.device
            && loaded.identity.device_digest() == captured.identity.device_digest()
            && loaded.identity.library_digest() == captured.identity.library_digest()
            && loaded.raw_preferences == captured.raw_preferences
            && loaded.plist_preferences == captured.plist_preferences
            && loaded.cdb == captured.cdb
            && loaded.artwork_db == captured.artwork_db
            && loaded.receipt_sha256 == captured.receipt_sha256
            && (a.database, a.wal) == (b.database, b.wal)
            && loaded.catalog.persisted_media_anchor() == captured.catalog.persisted_media_anchor()
    );
}

#[test]
fn saved_state_pins_refuse_before_catalog_parse_or_receipt_rewrite() {
    let t = Temp::new();
    let directory = t.0.join("saved");
    io::make_directory(&directory).unwrap();
    let bytes = b"historical receipt";
    io::write(&directory.join("state-receipt.json"), bytes).unwrap();
    let spec = SavedCurrent {
        directory: directory.clone(),
        receipt_sha256: io::hex(&[7; 32]),
    };
    assert!(current::load_current(&spec, "saved-device", &t.0).is_err());
    assert_eq!(
        fs::read(directory.join("state-receipt.json")).unwrap(),
        bytes
    );
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    // Pinned exactly, a receipt the codec refuses is still refused.
    let spec = SavedCurrent {
        receipt_sha256: io::hex(&io::sha(bytes)),
        ..spec
    };
    assert!(current::load_current(&spec, "saved-device", &t.0).is_err());
}

/// A phone a computer synced or one `rustunes init` prepared (no CDB), with
/// the ArtworkDB not asked for, absent or present, its receipt in either
/// form: each loads as the capture returned it.
#[test]
fn every_capture_loads_as_it_was_captured() {
    let t = Temp::new();
    for voice_memo in [Some(1), None] {
        for artwork_db in [
            DeviceFile::NotCaptured,
            DeviceFile::Absent,
            DeviceFile::Present(fixtures::artwork_db()),
        ] {
            for form in [Form::Capped, Form::Pretty] {
                let saved = t.saved(Shape {
                    voice_memo,
                    artwork_db: artwork_db.clone(),
                    form,
                    released: false,
                });
                assert_same(&load(&t, &saved).unwrap(), &saved.current);
            }
        }
    }
}

/// The directories 0.1.0 and 0.1.1 left: 15-key receipts that always bind
/// a CDB, and bind the ArtworkDB when the capture asked for it.
#[test]
fn a_capture_of_0_1_1_loads() {
    let t = Temp::new();
    for artwork_db in [
        DeviceFile::NotCaptured,
        DeviceFile::Present(fixtures::artwork_db()),
    ] {
        for form in [Form::Capped, Form::Pretty] {
            let saved = t.saved(Shape {
                artwork_db: artwork_db.clone(),
                form,
                released: true,
                ..Shape::synced()
            });
            let receipt = fs::read(receipt::RECEIPT.at(&saved.directory)).unwrap();
            assert!(!String::from_utf8(receipt).unwrap().contains("captured"));
            assert_same(&load(&t, &saved).unwrap(), &saved.current);
        }
    }
}

#[test]
fn a_file_the_receipt_records_none_of_must_not_be_there() {
    let t = Temp::new();
    let shape = |voice_memo, artwork_db| Shape {
        voice_memo,
        artwork_db,
        ..Shape::synced()
    };
    for (shape, file) in [
        (shape(None, DeviceFile::NotCaptured), receipt::CDB),
        (shape(Some(1), DeviceFile::Absent), receipt::ARTWORK_DB),
        (shape(Some(1), DeviceFile::NotCaptured), receipt::ARTWORK_DB),
        (shape(Some(1), DeviceFile::NotCaptured), receipt::WAL),
    ] {
        let saved = t.saved(shape);
        load(&t, &saved).unwrap();
        io::write(&file.at(&saved.directory), b"unbound").unwrap();
        assert!(load(&t, &saved).is_err(), "{}", file.path);
    }
}

#[test]
fn a_bound_file_missing_or_with_other_bytes_is_refused() {
    let t = Temp::new();
    for file in [
        receipt::PREFS,
        receipt::PREFS_PLIST,
        receipt::CDB,
        receipt::ARTWORK_DB,
        receipt::DATABASE,
    ] {
        for replace in [false, true] {
            let saved = t.saved(Shape {
                artwork_db: DeviceFile::Present(fixtures::artwork_db()),
                ..Shape::synced()
            });
            let path = file.at(&saved.directory);
            let mut bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            if replace {
                bytes[40] ^= 1;
                io::write(&path, &bytes).unwrap();
            }
            assert!(load(&t, &saved).is_err(), "{} {replace}", file.path);
        }
    }
}
