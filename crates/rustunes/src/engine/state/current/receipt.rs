//! The state receipt, `state-receipt.json`: what a Current capture writes
//! beside the files it copied, binding each by its SHA-256, and the layout
//! of that directory.
//!
//! A capture describes what it saw (`StateReceipt::describe`) and encodes it
//! in one of two forms: compact within the controlled capture's metadata
//! cap, or pretty. Either form has the keys in alphabetical order, as
//! serde_json writes a map, so its bytes are those captures wrote before
//! this codec. `decode` reads both forms, and the 15-key receipts rustunes
//! 0.1.0 and 0.1.1 wrote, which have no `artwork_database_captured`. It
//! refuses another schema, a missing or unknown key, a receipt that claims
//! a phone write or an atomic snapshot, a hash that is not 64 lowercase hex
//! digits or is zero, saved CDB anchors without a CDB hash or a CDB hash
//! without them, and an ArtworkDB hash the receipt says was not captured.
//!
//! A Current directory holds the receipt, the prefs pair, the iTunesCDB and
//! the ArtworkDB when the capture copied them, and the catalogue and its
//! log under `catalog/`. Each `CurrentFile` names one with the most bytes
//! it may hold; the capture and the saved Current reader share them, and
//! `bound_files` lists those a capture's receipt binds, for a copy.
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{Current, DeviceFile, hex};
use crate::engine::state::{journal::Identity, retained_files};
use music_protocol::catalogue::anchor::{self, AnchorState};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::transfer::{MAX_CATALOG, current_json_bytes};

const SCHEMA: &str = "rustunes.current-state";

/// A file of a Current directory: its path there and the most bytes it may
/// hold.
#[derive(Clone, Copy)]
pub(crate) struct CurrentFile {
    pub(crate) path: &'static str,
    pub(crate) cap: u64,
}

impl CurrentFile {
    pub(crate) fn at(self, directory: &Path) -> PathBuf {
        directory.join(self.path)
    }
}

/// The receipt. Either form of one is under 2 KiB.
pub(crate) const RECEIPT: CurrentFile = CurrentFile {
    path: "state-receipt.json",
    cap: 1024 * 1024,
};
/// The prefs pair (`iTunesPrefs` and its plist).
pub(crate) const PREFS: CurrentFile = CurrentFile {
    path: "iTunesPrefs",
    cap: 1024 * 1024,
};
pub(crate) const PREFS_PLIST: CurrentFile = CurrentFile {
    path: "iTunesPrefs.plist",
    cap: 1024 * 1024,
};
pub(crate) const CDB: CurrentFile = CurrentFile {
    path: "iTunesCDB",
    cap: anchor::MAX_INPUT_BYTES as u64,
};
pub(crate) const ARTWORK_DB: CurrentFile = CurrentFile {
    path: "ArtworkDB",
    cap: 32 * 1024 * 1024,
};
/// The folder holding the catalogue and its log.
pub(crate) const CATALOG: &str = "catalog";
/// The catalogue and its log, each at most what one read from the phone
/// may hold.
pub(crate) const DATABASE: CurrentFile = CurrentFile {
    path: "catalog/MediaLibrary.sqlitedb",
    cap: MAX_CATALOG,
};
pub(crate) const WAL: CurrentFile = CurrentFile {
    path: "catalog/MediaLibrary.sqlitedb-wal",
    cap: MAX_CATALOG,
};

/// Every file a capture of `current` left in its directory, with the
/// SHA-256 its receipt binds: the receipt, the prefs pair, the CDB and the
/// ArtworkDB when the phone had them and the capture copied them, the
/// catalogue, and its log when it had one.
pub(crate) fn bound_files(current: &Current) -> Vec<(CurrentFile, [u8; 32])> {
    let sha = |bytes: &[u8]| -> [u8; 32] { Sha256::digest(bytes).into() };
    let hashes = current.catalog.source_hashes();
    let mut files = vec![
        (RECEIPT, current.receipt_sha256),
        (PREFS, sha(&current.raw_preferences)),
        (PREFS_PLIST, sha(&current.plist_preferences)),
    ];
    files.extend(current.cdb.bytes().map(|bytes| (CDB, sha(bytes))));
    files.extend(
        current
            .artwork_db
            .bytes()
            .map(|bytes| (ARTWORK_DB, sha(bytes))),
    );
    files.push((DATABASE, hashes.database));
    files.extend(hashes.wal.map(|hash| (WAL, hash)));
    files
}

/// The two forms a capture writes a receipt in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Form {
    /// Compact, within the controlled capture's 16 KiB metadata cap.
    Capped,
    /// Indented by two spaces, as an uncontrolled capture writes it.
    Pretty,
}

/// What a receipt records of a file the phone may not have had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Recorded {
    /// The capture copied the file, whose bytes have this SHA-256.
    Hash([u8; 32]),
    /// The capture looked, and the phone had none.
    Absent,
    /// The capture did not look.
    NotCaptured,
}

/// The receipt of one capture. Its fields are declared in alphabetical
/// order: a struct serializes them in that order, as serde_json writes the
/// keys of a map.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateReceipt {
    /// Absent from the receipts 0.1.0 and 0.1.1 wrote, whose capture always
    /// found the ArtworkDB it asked for.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "written"
    )]
    artwork_database_captured: Option<bool>,
    #[serde(deserialize_with = "present")]
    artwork_database_sha256: Option<Sha>,
    atomic_snapshot_claimed: bool,
    #[serde(deserialize_with = "present")]
    cdb_sha256: Option<Sha>,
    database_sha256: Sha,
    device_sha256: Sha,
    library_sha256: Sha,
    media_anchor: u32,
    phone_writes: u64,
    plist_preferences_sha256: Sha,
    raw_preferences_sha256: Sha,
    #[serde(deserialize_with = "present")]
    saved_cdb_media_anchor: Option<u32>,
    #[serde(deserialize_with = "present")]
    saved_cdb_voice_memo_anchor: Option<u32>,
    schema: String,
    track_count: u64,
    #[serde(deserialize_with = "present")]
    wal_sha256: Option<Sha>,
}

impl StateReceipt {
    /// The receipt of a capture that saw these: the device and library,
    /// the catalogue, the prefs pair, the CDB and the ArtworkDB, and the
    /// saved anchors of the CDB's bytes (None when it has none).
    pub(crate) fn describe(
        identity: &Identity,
        catalog: &Catalog,
        raw_preferences: &[u8],
        plist_preferences: &[u8],
        cdb: &DeviceFile,
        artwork_db: &DeviceFile,
        anchors: Option<&AnchorState>,
    ) -> Self {
        let sha = |bytes: &[u8]| Sha(Sha256::digest(bytes).into());
        let (database_sha256, wal_sha256, media_anchor, track_count) = catalogue_keys(catalog);
        Self {
            artwork_database_captured: Some(artwork_db.captured()),
            artwork_database_sha256: artwork_db.bytes().map(sha),
            atomic_snapshot_claimed: false,
            cdb_sha256: cdb.bytes().map(sha),
            database_sha256,
            device_sha256: Sha(identity.device_digest()),
            library_sha256: Sha(identity.library_digest()),
            media_anchor,
            phone_writes: 0,
            plist_preferences_sha256: sha(plist_preferences),
            raw_preferences_sha256: sha(raw_preferences),
            saved_cdb_media_anchor: anchors.map(AnchorState::saved_media_anchor),
            saved_cdb_voice_memo_anchor: anchors.map(AnchorState::saved_voice_memo_anchor),
            schema: SCHEMA.into(),
            track_count,
            wal_sha256,
        }
    }

    pub(crate) fn encode(&self, form: Form) -> Result<Vec<u8>> {
        Ok(match form {
            Form::Capped => current_json_bytes(self)?,
            Form::Pretty => serde_json::to_vec_pretty(self)?,
        })
    }

    /// The receipt in `bytes`, in either form and with 15 or 16 keys.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let receipt: Self = serde_json::from_slice(bytes)?;
        ensure!(
            receipt.schema == SCHEMA,
            "the state receipt has another schema"
        );
        ensure!(
            receipt.phone_writes == 0 && !receipt.atomic_snapshot_claimed,
            "the state receipt claims a phone write or an atomic snapshot"
        );
        ensure!(
            receipt.cdb_sha256.is_some() == receipt.saved_cdb_media_anchor.is_some()
                && receipt.cdb_sha256.is_some() == receipt.saved_cdb_voice_memo_anchor.is_some(),
            "the state receipt's CDB hash and saved anchors disagree"
        );
        ensure!(
            receipt.artwork_database_sha256.is_none()
                || receipt.artwork_database_captured != Some(false),
            "the state receipt binds an ArtworkDB it did not capture"
        );
        Ok(receipt)
    }

    pub(crate) fn raw_preferences_sha256(&self) -> [u8; 32] {
        self.raw_preferences_sha256.0
    }

    pub(crate) fn plist_preferences_sha256(&self) -> [u8; 32] {
        self.plist_preferences_sha256.0
    }

    /// The CDB: its hash, or absent (every capture looks for one).
    pub(crate) fn cdb(&self) -> Recorded {
        self.cdb_sha256
            .map_or(Recorded::Absent, |sha| Recorded::Hash(sha.0))
    }

    /// The ArtworkDB. A receipt of 0.1.0 or 0.1.1 that records no hash did
    /// not capture it.
    pub(crate) fn artwork_db(&self) -> Recorded {
        match (self.artwork_database_sha256, self.artwork_database_captured) {
            (Some(sha), _) => Recorded::Hash(sha.0),
            (None, Some(true)) => Recorded::Absent,
            (None, _) => Recorded::NotCaptured,
        }
    }

    pub(crate) fn database_sha256(&self) -> [u8; 32] {
        self.database_sha256.0
    }

    pub(crate) fn wal_sha256(&self) -> Option<[u8; 32]> {
        self.wal_sha256.map(|sha| sha.0)
    }

    /// Whether the receipt names this device and library.
    pub(crate) fn describes_identity(&self, identity: &Identity) -> bool {
        self.device_sha256.0 == identity.device_digest()
            && self.library_sha256.0 == identity.library_digest()
    }

    /// Whether the receipt describes this catalogue: its database and log,
    /// media anchor and track count.
    pub(crate) fn describes_catalog(&self, catalog: &Catalog) -> bool {
        (
            self.database_sha256,
            self.wal_sha256,
            self.media_anchor,
            self.track_count,
        ) == catalogue_keys(catalog)
    }

    /// Whether the receipt records these saved anchors of its CDB, or none
    /// with None.
    pub(crate) fn describes_saved_anchors(&self, anchors: Option<&AnchorState>) -> bool {
        (
            self.saved_cdb_media_anchor,
            self.saved_cdb_voice_memo_anchor,
        ) == (
            anchors.map(AnchorState::saved_media_anchor),
            anchors.map(AnchorState::saved_voice_memo_anchor),
        )
    }

    /// The receipt as 0.1.0 and 0.1.1 wrote it, without
    /// `artwork_database_captured`.
    #[cfg(test)]
    pub(crate) fn released(mut self) -> Self {
        self.artwork_database_captured = None;
        self
    }
}

fn catalogue_keys(catalog: &Catalog) -> (Sha, Option<Sha>, u32, u64) {
    let hashes = catalog.source_hashes();
    (
        Sha(hashes.database),
        hashes.wal.map(Sha),
        catalog.persisted_media_anchor(),
        catalog.tracks().len() as u64,
    )
}

/// A SHA-256 as a receipt writes it: 64 lowercase hex digits, not all zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sha([u8; 32]);

impl Serialize for Sha {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex(&self.0))
    }
}

impl<'de> Deserialize<'de> for Sha {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        retained_files::digest(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// A key every receipt has, whose value may be null.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<T, D::Error> {
    T::deserialize(deserializer)
}

/// A key the receipts of 0.1.0 and 0.1.1 lack, and which is never null
/// where it is written.
fn written<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests;
