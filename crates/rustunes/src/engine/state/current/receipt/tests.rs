//! The codec against the receipts captures wrote before it: byte for byte
//! in both forms, and the 15-key receipts of 0.1.0 and 0.1.1.
use super::*;
use crate::engine::state::{current::fixtures, device_files};
use music_protocol::catalogue::identity;
use music_protocol::test_support::catalogue::{Fixture as Catalogue, collection_catalog};
use serde_json::{Map, Value, json};

/// The receipt as the capture built it before this codec: the oracle
/// every 16-key encoding matches byte for byte.
fn historical(
    identity: &Identity,
    catalog: &Catalog,
    raw_preferences: &[u8],
    plist_preferences: &[u8],
    cdb: &DeviceFile,
    artwork_db: &DeviceFile,
    anchors: Option<&AnchorState>,
) -> Value {
    json!({
        "schema":"rustunes.current-state", "device_sha256":hex(&identity.device_digest()),
        "library_sha256":hex(&identity.library_digest()), "database_sha256":hex(&catalog.source_hashes().database),
        "wal_sha256":catalog.source_hashes().wal.map(|h|hex(&h)), "media_anchor":catalog.persisted_media_anchor(),
        "track_count":catalog.tracks().len(), "raw_preferences_sha256":hex(&Sha256::digest(raw_preferences)),
        "plist_preferences_sha256":hex(&Sha256::digest(plist_preferences)), "cdb_sha256":cdb.bytes().map(|bytes|hex(&Sha256::digest(bytes))),
        "artwork_database_sha256":artwork_db.bytes().map(|bytes|hex(&Sha256::digest(bytes))),
        "artwork_database_captured":artwork_db.captured(),
        "saved_cdb_media_anchor":anchors.as_ref().map(|a|a.saved_media_anchor()), "saved_cdb_voice_memo_anchor":anchors.as_ref().map(|a|a.saved_voice_memo_anchor()),
        "atomic_snapshot_claimed":false, "phone_writes":0
    })
}

/// The receipt as 0.1.0 (ca80a09), 0.1.1 (4511f49) and 3de3e0e built it,
/// whose capture always found the CDB, and the ArtworkDB when it asked.
fn released(
    identity: &Identity,
    catalog: &Catalog,
    raw_preferences: &[u8],
    plist_preferences: &[u8],
    cdb: &[u8],
    artwork_db: Option<&[u8]>,
    anchors: &AnchorState,
) -> Value {
    json!({
        "schema":"rustunes.current-state", "device_sha256":hex(&identity.device_digest()),
        "library_sha256":hex(&identity.library_digest()), "database_sha256":hex(&catalog.source_hashes().database),
        "wal_sha256":catalog.source_hashes().wal.map(|h|hex(&h)), "media_anchor":catalog.persisted_media_anchor(),
        "track_count":catalog.tracks().len(), "raw_preferences_sha256":hex(&Sha256::digest(raw_preferences)),
        "plist_preferences_sha256":hex(&Sha256::digest(plist_preferences)), "cdb_sha256":hex(&Sha256::digest(cdb)),
        "artwork_database_sha256":artwork_db.as_ref().map(|bytes|hex(&Sha256::digest(bytes))),
        "saved_cdb_media_anchor":anchors.saved_media_anchor(), "saved_cdb_voice_memo_anchor":anchors.saved_voice_memo_anchor(),
        "atomic_snapshot_claimed":false, "phone_writes":0
    })
}

/// The bytes the capture wrote of `value` in `form`.
fn written_as(value: &Value, form: Form) -> Vec<u8> {
    match form {
        Form::Capped => current_json_bytes(value).unwrap(),
        Form::Pretty => serde_json::to_vec_pretty(value).unwrap(),
    }
}

const FORMS: [Form; 2] = [Form::Capped, Form::Pretty];

fn identity() -> Identity {
    let library = identity::parse(&fixtures::prefs()).unwrap();
    Identity::new(fixtures::DEVICE, &library).unwrap()
}

/// A catalogue without a log, and one with.
fn catalogs() -> [Catalog; 2] {
    let with_wal = Catalogue::new(true).read(true).unwrap();
    assert!(with_wal.source_hashes().wal.is_some());
    [collection_catalog(13), with_wal]
}

/// Each way a capture may hold a file: not asked for, absent, `bytes`.
fn device_files(bytes: Vec<u8>) -> [DeviceFile; 3] {
    [
        DeviceFile::NotCaptured,
        DeviceFile::Absent,
        DeviceFile::Present(bytes),
    ]
}

/// Every receipt a capture may describe, with what it described.
fn every_receipt() -> Vec<(StateReceipt, Value, DeviceFile, DeviceFile)> {
    let identity = identity();
    let (raw, plist) = (fixtures::prefs(), b"plist".to_vec());
    let mut out = Vec::new();
    for catalog in catalogs() {
        for cdb in device_files(fixtures::cdb(catalog.database_pid(), 1)) {
            let anchors = cdb.bytes().map(|bytes| anchor::parse(bytes).unwrap());
            for artwork_db in device_files(fixtures::artwork_db()) {
                let receipt = StateReceipt::describe(
                    &identity,
                    &catalog,
                    &raw,
                    &plist,
                    &cdb,
                    &artwork_db,
                    anchors.as_ref(),
                );
                let oracle = historical(
                    &identity,
                    &catalog,
                    &raw,
                    &plist,
                    &cdb,
                    &artwork_db,
                    anchors.as_ref(),
                );
                out.push((receipt, oracle, cdb.clone(), artwork_db));
            }
        }
    }
    assert_eq!(out.len(), 18);
    out
}

#[test]
fn encodes_byte_for_byte_as_the_capture_did() {
    for (receipt, oracle, ..) in every_receipt() {
        for form in FORMS {
            assert_eq!(
                String::from_utf8(receipt.encode(form).unwrap()).unwrap(),
                String::from_utf8(written_as(&oracle, form)).unwrap(),
                "{form:?}"
            );
        }
    }
}

#[test]
fn the_released_form_is_what_0_1_1_wrote() {
    let identity = identity();
    let (raw, plist) = (fixtures::prefs(), b"plist".to_vec());
    for catalog in catalogs() {
        let cdb = fixtures::cdb(catalog.database_pid(), 1);
        let anchors = anchor::parse(&cdb).unwrap();
        for artwork_db in [None, Some(fixtures::artwork_db())] {
            let receipt = StateReceipt::describe(
                &identity,
                &catalog,
                &raw,
                &plist,
                &DeviceFile::Present(cdb.clone()),
                &artwork_db
                    .clone()
                    .map_or(DeviceFile::NotCaptured, DeviceFile::Present),
                Some(&anchors),
            )
            .released();
            let oracle = released(
                &identity,
                &catalog,
                &raw,
                &plist,
                &cdb,
                artwork_db.as_deref(),
                &anchors,
            );
            for form in FORMS {
                let bytes = receipt.encode(form).unwrap();
                assert_eq!(bytes, written_as(&oracle, form), "{form:?}");
                assert_eq!(StateReceipt::decode(&bytes).unwrap(), receipt);
            }
        }
    }
}

#[test]
fn decodes_what_it_encodes_and_records_each_file_as_captured() {
    let identity = identity();
    let other = Identity::new(
        "another-device",
        &identity::parse(&fixtures::prefs()).unwrap(),
    )
    .unwrap();
    let recorded = |file: &DeviceFile, none: Recorded| match file.bytes() {
        Some(bytes) => Recorded::Hash(Sha256::digest(bytes).into()),
        None if file.captured() => Recorded::Absent,
        None => none,
    };
    for (receipt, _, cdb, artwork_db) in every_receipt() {
        for form in FORMS {
            let decoded = StateReceipt::decode(&receipt.encode(form).unwrap()).unwrap();
            assert_eq!(decoded, receipt);
            // A capture always looks for the CDB; one it did not ask for
            // reads as absent.
            assert_eq!(decoded.cdb(), recorded(&cdb, Recorded::Absent));
            assert_eq!(
                decoded.artwork_db(),
                recorded(&artwork_db, Recorded::NotCaptured)
            );
            assert!(decoded.describes_identity(&identity));
            assert!(!decoded.describes_identity(&other));
            let anchors = cdb.bytes().map(|bytes| anchor::parse(bytes).unwrap());
            assert!(decoded.describes_saved_anchors(anchors.as_ref()));
            if let Some(anchors) = &anchors {
                assert!(!decoded.describes_saved_anchors(None));
                let moved = anchor::parse(&fixtures::cdb(anchors.database_pid(), 2)).unwrap();
                assert!(!decoded.describes_saved_anchors(Some(&moved)));
            }
        }
    }
    let [plain, logged] = catalogs();
    let receipt = |catalog: &Catalog| {
        StateReceipt::describe(
            &identity,
            catalog,
            b"raw",
            b"plist",
            &DeviceFile::Absent,
            &DeviceFile::NotCaptured,
            None,
        )
    };
    assert!(receipt(&plain).describes_catalog(&plain));
    assert!(!receipt(&plain).describes_catalog(&logged));
    assert!(!receipt(&plain).describes_catalog(&collection_catalog(14)));
}

/// What 0.1.1's controlled capture of an insert batch wrote: compact, with
/// the ArtworkDB and a log.
const RELEASED_CAPPED: &str = r#"{"artwork_database_sha256":"a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1","atomic_snapshot_claimed":false,"cdb_sha256":"c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1","database_sha256":"d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1","device_sha256":"dededededededededededededededededededededededededededededededede","library_sha256":"1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b","media_anchor":412,"phone_writes":0,"plist_preferences_sha256":"9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f","raw_preferences_sha256":"7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a","saved_cdb_media_anchor":397,"saved_cdb_voice_memo_anchor":1,"schema":"rustunes.current-state","track_count":120,"wal_sha256":"3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e3e"}"#;

/// What 0.1.1's uncontrolled capture (a refresh) wrote: pretty, without
/// the ArtworkDB or a log.
const RELEASED_PRETTY: &str = r#"{
  "artwork_database_sha256": null,
  "atomic_snapshot_claimed": false,
  "cdb_sha256": "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
  "database_sha256": "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1",
  "device_sha256": "dededededededededededededededededededededededededededededededede",
  "library_sha256": "1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b",
  "media_anchor": 412,
  "phone_writes": 0,
  "plist_preferences_sha256": "9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f9f",
  "raw_preferences_sha256": "7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a",
  "saved_cdb_media_anchor": 397,
  "saved_cdb_voice_memo_anchor": 1,
  "schema": "rustunes.current-state",
  "track_count": 120,
  "wal_sha256": null
}"#;

#[test]
fn reads_the_receipts_0_1_1_wrote() {
    let digest = |byte: u8| [byte; 32];
    for (text, form, artwork_db, wal) in [
        (
            RELEASED_CAPPED,
            Form::Capped,
            Recorded::Hash(digest(0xa1)),
            Some(digest(0x3e)),
        ),
        (RELEASED_PRETTY, Form::Pretty, Recorded::NotCaptured, None),
    ] {
        let receipt = StateReceipt::decode(text.as_bytes()).unwrap();
        assert_eq!(receipt.cdb(), Recorded::Hash(digest(0xc1)));
        assert_eq!(receipt.artwork_db(), artwork_db);
        assert_eq!(receipt.raw_preferences_sha256(), digest(0x7a));
        assert_eq!(receipt.plist_preferences_sha256(), digest(0x9f));
        assert_eq!(receipt.database_sha256(), digest(0xd1));
        assert_eq!(receipt.wal_sha256(), wal);
        assert_eq!(
            (receipt.media_anchor, receipt.track_count),
            (412, 120),
            "{text}"
        );
        assert_eq!(
            (
                receipt.saved_cdb_media_anchor,
                receipt.saved_cdb_voice_memo_anchor
            ),
            (Some(397), Some(1))
        );
        // Encoded again, it is what 0.1.1 wrote.
        assert_eq!(receipt.encode(form).unwrap(), text.as_bytes());
    }
}

/// A receipt every rule accepts, as a JSON object to edit.
fn object() -> Map<String, Value> {
    let catalog = collection_catalog(13);
    let cdb = fixtures::cdb(catalog.database_pid(), 1);
    let anchors = anchor::parse(&cdb).unwrap();
    let receipt = StateReceipt::describe(
        &identity(),
        &catalog,
        b"raw",
        b"plist",
        &DeviceFile::Present(cdb),
        &DeviceFile::Present(fixtures::artwork_db()),
        Some(&anchors),
    );
    match serde_json::to_value(&receipt).unwrap() {
        Value::Object(object) => object,
        _ => unreachable!(),
    }
}

fn decode(object: Map<String, Value>) -> Result<StateReceipt> {
    StateReceipt::decode(&serde_json::to_vec(&object).unwrap())
}

/// `object` with `edit`, refused.
fn refused(edit: impl FnOnce(&mut Map<String, Value>)) {
    let mut object = object();
    edit(&mut object);
    let text = serde_json::to_string(&object).unwrap();
    assert!(decode(object).is_err(), "{text}");
}

#[test]
fn refuses_another_schema() {
    decode(object()).unwrap();
    refused(|o| {
        o.insert("schema".into(), "rustunes.capture-attempt".into());
    });
    refused(|o| {
        o.insert("schema".into(), Value::Null);
    });
}

#[test]
fn refuses_a_missing_or_unknown_key_or_a_null_capture_flag() {
    for key in object().keys() {
        if key != "artwork_database_captured" {
            refused(|o| {
                o.remove(key);
            });
        }
    }
    refused(|o| {
        o.insert("selected_attempt".into(), 1.into());
    });
    refused(|o| {
        o.insert("artwork_database_captured".into(), Value::Null);
    });
}

#[test]
fn refuses_a_phone_write_or_an_atomic_snapshot() {
    refused(|o| {
        o.insert("phone_writes".into(), 1.into());
    });
    refused(|o| {
        o.insert("phone_writes".into(), json!(0.0));
    });
    refused(|o| {
        o.insert("atomic_snapshot_claimed".into(), true.into());
    });
}

#[test]
fn refuses_a_hash_that_is_not_canonical_or_is_zero() {
    let keys = [
        "artwork_database_sha256",
        "cdb_sha256",
        "database_sha256",
        "device_sha256",
        "library_sha256",
        "plist_preferences_sha256",
        "raw_preferences_sha256",
    ];
    for key in keys {
        for bad in [
            json!("AB".repeat(32)),
            json!("ab".repeat(31)),
            json!("ab".repeat(33)),
            json!("0".repeat(64)),
            json!("zz".repeat(32)),
            json!(1),
        ] {
            refused(|o| {
                o.insert(key.into(), bad);
            });
        }
    }
    let mut object = object();
    object.insert("wal_sha256".into(), json!("0".repeat(64)));
    assert!(decode(object).is_err());
}

#[test]
fn refuses_saved_anchors_without_a_cdb_and_a_cdb_without_them() {
    refused(|o| {
        o.insert("cdb_sha256".into(), Value::Null);
    });
    refused(|o| {
        o.insert("saved_cdb_media_anchor".into(), Value::Null);
    });
    refused(|o| {
        o.insert("saved_cdb_voice_memo_anchor".into(), Value::Null);
    });
    let mut object = object();
    for key in [
        "cdb_sha256",
        "saved_cdb_media_anchor",
        "saved_cdb_voice_memo_anchor",
    ] {
        object.insert(key.into(), Value::Null);
    }
    assert_eq!(decode(object).unwrap().cdb(), Recorded::Absent);
}

#[test]
fn refuses_an_artwork_db_hash_it_did_not_capture() {
    refused(|o| {
        o.insert("artwork_database_captured".into(), false.into());
    });
}

#[test]
fn reads_the_artwork_db_as_recorded_with_or_without_the_capture_flag() {
    let hash = Recorded::Hash(Sha256::digest(fixtures::artwork_db()).into());
    for (sha256, captured, expected) in [
        (true, Some(true), hash),
        (true, None, hash),
        (false, Some(true), Recorded::Absent),
        (false, Some(false), Recorded::NotCaptured),
        (false, None, Recorded::NotCaptured),
    ] {
        let mut object = object();
        if !sha256 {
            object.insert("artwork_database_sha256".into(), Value::Null);
        }
        match captured {
            Some(captured) => object.insert("artwork_database_captured".into(), captured.into()),
            None => object.remove("artwork_database_captured"),
        };
        assert_eq!(decode(object).unwrap().artwork_db(), expected);
    }
}

#[test]
fn names_each_file_where_the_capture_writes_it() {
    for (device, file) in [
        (device_files::PREFS, PREFS),
        (device_files::PREFS_PLIST, PREFS_PLIST),
        (device_files::CDB, CDB),
        (device_files::ARTWORK_DB, ARTWORK_DB),
    ] {
        assert_eq!(device.rsplit('/').next(), Some(file.path));
    }
    for (device, file) in [
        (device_files::DATABASE, DATABASE),
        (device_files::DATABASE_WAL, WAL),
    ] {
        let name = device.rsplit('/').next().unwrap();
        assert_eq!(file.path, format!("{CATALOG}/{name}"));
    }
}
