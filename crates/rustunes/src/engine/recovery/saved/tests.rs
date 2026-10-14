use super::*;
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
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
}
