//! Files a capture copies from the phone, and a Current directory as a
//! capture leaves one, for tests.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use super::{
    Current, DeviceFile,
    receipt::{self, Form, StateReceipt},
};
use crate::engine::state::{journal::Identity, retained_files as io};
use music_protocol::catalogue::{anchor, identity};

/// The device every fixture directory was captured from.
pub(crate) const DEVICE: &str = "synthetic";

/// A raw iTunesPrefs (`frpd` 1.25) of catalogue 1 and user 2.
pub(crate) fn prefs() -> Vec<u8> {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25_u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&1_u64.to_le_bytes());
    raw[20..28].copy_from_slice(&2_u64.to_le_bytes());
    raw
}

/// A copied iTunesCDB of `database_pid` whose saved anchors are Media 7 and
/// VoiceMemo `voice_memo`.
pub(crate) fn cdb(database_pid: u64, voice_memo: u32) -> Vec<u8> {
    let mut bytes = vec![0; 0xf4 + 0x60 + 0x30];
    for (offset, value) in [
        (0, 0x6462_686d),
        (4, 0xf4),
        (8, 388),
        (12, 1),
        (16, 117),
        (20, 1),
        (0xf4, 0x6473_686d),
        (0xf8, 0x60),
        (0xfc, 0x90),
        (0x100, 11),
        (0x154, 7),
        (0x15c, voice_memo),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&u32::to_le_bytes(value));
    }
    bytes[0x48..0x50].copy_from_slice(&database_pid.to_le_bytes());
    bytes
}

/// An ArtworkDB with no images, whose next record ID is 100.
pub(crate) fn artwork_db() -> Vec<u8> {
    let mut bytes = vec![0; 132 + 96 + 92];
    for (offset, value) in [
        (0, 0x6466_686d),
        (4, 132),
        (8, 320),
        (16, 6),
        (20, 1),
        (28, 100),
        (132, 0x6473_686d),
        (136, 96),
        (140, 188),
        (144, 1),
        (228, 0x696c_686d),
        (232, 92),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&u32::to_le_bytes(value));
    }
    bytes
}

/// What a capture found on the phone, and how it wrote its receipt.
#[derive(Clone)]
pub(crate) struct Shape {
    /// The saved VoiceMemo anchor of a CDB of the captured catalogue; None
    /// when the phone had no CDB.
    pub(crate) voice_memo: Option<u32>,
    pub(crate) artwork_db: DeviceFile,
    pub(crate) form: Form,
    /// The receipt has the 15 keys 0.1.0 and 0.1.1 wrote.
    pub(crate) released: bool,
}

impl Shape {
    /// A phone a computer synced: a CDB at VoiceMemo anchor 1, the
    /// ArtworkDB not asked for, the receipt of a controlled capture.
    pub(crate) fn synced() -> Self {
        Self {
            voice_memo: Some(1),
            artwork_db: DeviceFile::NotCaptured,
            form: Form::Capped,
            released: false,
        }
    }
}

pub(crate) struct Saved {
    pub(crate) directory: PathBuf,
    /// What the capture returned.
    pub(crate) current: Current,
}

/// Write at `directory` (new, in a canonical 0700 directory) what a capture
/// of `shape` leaves: a 13-track catalogue at media anchor 13 without a
/// log, the prefs pair, the CDB and ArtworkDB the phone had, and the
/// receipt binding them.
pub(crate) fn write(directory: &Path, shape: Shape) -> Saved {
    io::make_directory(directory).unwrap();
    io::make_directory(&directory.join(receipt::CATALOG)).unwrap();
    let database = receipt::DATABASE.at(directory);
    let catalog = music_protocol::test_support::catalogue::write_modified_collection_catalog(
        13, "", &database,
    );
    // Private whatever the umask, as a reader requires.
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    let prefs = prefs();
    io::write(&receipt::PREFS.at(directory), &prefs).unwrap();
    io::write(&receipt::PREFS_PLIST.at(directory), &prefs).unwrap();
    let cdb = match shape.voice_memo {
        Some(voice_memo) => DeviceFile::Present(cdb(catalog.database_pid(), voice_memo)),
        None => DeviceFile::Absent,
    };
    for (file, copied) in [
        (receipt::CDB, &cdb),
        (receipt::ARTWORK_DB, &shape.artwork_db),
    ] {
        if let Some(bytes) = copied.bytes() {
            io::write(&file.at(directory), bytes).unwrap();
        }
    }
    let library = identity::parse(&prefs).unwrap();
    let identity = Identity::new(DEVICE, &library).unwrap();
    let anchors = cdb.bytes().map(|bytes| anchor::parse(bytes).unwrap());
    let mut described = StateReceipt::describe(
        &identity,
        &catalog,
        &prefs,
        &prefs,
        &cdb,
        &shape.artwork_db,
        anchors.as_ref(),
    );
    if shape.released {
        described = described.released();
    }
    let bytes = described.encode(shape.form).unwrap();
    io::write(&receipt::RECEIPT.at(directory), &bytes).unwrap();
    Saved {
        directory: directory.to_owned(),
        current: Current {
            device: DEVICE.into(),
            identity,
            library,
            catalog,
            raw_preferences: prefs.clone(),
            plist_preferences: prefs,
            cdb,
            artwork_db: shape.artwork_db,
            receipt_sha256: io::sha(&bytes),
        },
    }
}
