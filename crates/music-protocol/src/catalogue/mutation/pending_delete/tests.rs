use super::*;
use crate::test_support::catalogue::{Fixture, modified_collection_catalog};
use plist::Value;
use std::{collections::BTreeSet, io::Cursor, time::UNIX_EPOCH};

const DEST: &str = "iTunes_Control/Music/F39/OMJJ.m4a";
const PENDING: &str = "UPDATE item SET base_location_id=0;
    UPDATE item_extra SET location='',file_size=0;
    UPDATE item_state SET has_non_purgeable_asset=0;";
fn catalog(sql: &str) -> Catalog {
    modified_collection_catalog(140, &format!("{PENDING}{sql}"))
}
fn prepare(catalog: &Catalog) -> Result<PendingDelete> {
    PendingDelete::prepare(catalog, &catalog.tracks()[0], DEST, UNIX_EPOCH)
}

#[test]
fn pending_delete_genuine_parser_emits_only_database_update_and_single_delete() {
    let catalog = catalog("");
    let deletion = prepare(&catalog).unwrap();
    deletion.ensure_current(&catalog).unwrap();
    assert_eq!(deletion.target_sync_id(), u64::MAX);
    assert_eq!(deletion.target_asset_id(), "-1");
    assert_eq!(deletion.base_anchor(), 140);
    assert_eq!(deletion.revision(), 141);
    assert_eq!(deletion.reserved_destination(), DEST);
    let bytes = deletion.operation_bytes();
    assert!(bytes.starts_with(b"bplist00"));
    assert_eq!(bytes, prepare(&catalog).unwrap().operation_bytes());
    let value = Value::from_reader(Cursor::new(bytes)).unwrap();
    let envelope = value.as_dictionary().unwrap();
    assert_eq!(
        envelope.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["revision", "timestamp", "operations"])
    );
    assert_eq!(envelope["revision"].as_unsigned_integer(), Some(141));
    assert_eq!(envelope["timestamp"].as_date(), Some(UNIX_EPOCH.into()));
    let operations = envelope["operations"].as_array().unwrap();
    assert_eq!(operations.len(), 2);
    let db = operations[0].as_dictionary().unwrap();
    assert_eq!(
        db.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["operation", "pid", "db_info"])
    );
    assert_eq!(db["operation"].as_string(), Some("update_db_info"));
    assert_eq!(db["pid"].as_signed_integer(), Some(-10));
    let info = db["db_info"].as_dictionary().unwrap();
    assert_eq!(
        info.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["primary_container_pid"])
    );
    assert_eq!(info["primary_container_pid"].as_signed_integer(), Some(-11));
    let own = operations[1].as_dictionary().unwrap();
    assert_eq!(
        own.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["operation", "pid"])
    );
    assert_eq!(own["operation"].as_string(), Some("delete_track"));
    assert_eq!(own["pid"].as_signed_integer(), Some(-1));
}

#[test]
fn pending_delete_zero_metadata_size_does_not_admit_advertised_size_profile() {
    prepare(&catalog("")).unwrap();
    // Existing pending-audio fixtures advertise source size. They are valid
    // for that distinct profile, but not this zero-size profile.
    assert!(prepare(&catalog("UPDATE item_extra SET file_size=148305;")).is_err());
}

#[test]
fn pending_delete_exact_expected_row_rejects_identity_metadata_and_playback_substitution() {
    let catalog = catalog("");
    for change in 0..10 {
        let mut expected = catalog.tracks()[0].clone();
        match change {
            0 => expected.sync_id = 0,
            1 => expected.internal_id += 1,
            2 => expected.album_sync_id ^= 1,
            3 => expected.artist_sync_id ^= 1,
            4 => expected.metadata.title.push('x'),
            5 => expected.metadata.album_artist.push('x'),
            6 => expected.metadata.file_size = 148305,
            7 => expected.playback.play_count_recent += 1,
            8 => expected.afc_path = Some(DEST.into()),
            _ => expected.availability.has_non_purgeable_asset = true,
        }
        assert!(PendingDelete::prepare(&catalog, &expected, DEST, UNIX_EPOCH).is_err());
    }
}

#[test]
fn pending_delete_and_ordinary_installed_delete_keep_distinct_target_contracts() {
    let installed = crate::test_support::catalogue::collection_catalog(140);
    assert!(prepare(&installed).is_err());
    assert!(crate::catalogue::mutation::delete(&installed, u64::MAX, UNIX_EPOCH).is_ok());
    let pending = catalog("");
    prepare(&pending).unwrap();
    assert!(crate::catalogue::mutation::delete(&pending, u64::MAX, UNIX_EPOCH).is_err());
    assert!(
        crate::catalogue::mutation::title(&pending, u64::MAX, "Next", "Next", UNIX_EPOCH).is_err()
    );
}

#[test]
fn pending_delete_unfilled_flags_are_required_from_actual_parser_rows() {
    for sql in [
        "UPDATE item_state SET is_placeholder=1;",
        "UPDATE item_state SET is_library_added=0;",
        "UPDATE item_state SET is_supported_type=0;",
        "UPDATE item SET base_location_id=51; UPDATE item_extra SET location='OMJJ.m4a';",
    ] {
        assert!(prepare(&catalog(sql)).is_err(), "{sql}");
    }

    // An asset flag without a location is invalid catalogue input, so the
    // parser must reject it before PendingDelete can receive a projected row.
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch(PENDING).unwrap();
    fixture
        .connection
        .execute_batch("UPDATE item_state SET has_non_purgeable_asset=1;")
        .unwrap();
    let error = fixture
        .read(false)
        .err()
        .expect("parser must reject an asset flag without a location");
    assert_eq!(error.to_string(), "catalog asset flag has no location");
}

#[test]
fn pending_delete_destination_requires_canonical_unreferenced_alac_path() {
    let catalog = catalog("");
    for path in [
        "",
        "/iTunes_Control/Music/F39/OMJJ.m4a",
        "iTunes_Control/Music/F3/OMJJ.m4a",
        "iTunes_Control/Music/F39/../OMJJ.m4a",
        "iTunes_Control/Music/F39/OMJJ.mp3",
        "iTunes_Control/Music/F39/.m4a",
        "iTunes_Control/Music/F39/OMJJ.M4A",
        "iTunes_Control/Music/F39/OMJJ.m4a/",
        "iTunes_Control/Music/F39/OMJJ.m4a\0",
    ] {
        assert!(PendingDelete::prepare(&catalog, &catalog.tracks()[0], path, UNIX_EPOCH).is_err());
    }
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch(PENDING).unwrap();
    fixture.connection.execute_batch("INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,51,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
        INSERT INTO item_extra SELECT 2,title,'omjj.m4a',total_time_ms,12345,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;
        INSERT INTO item_store VALUES(2,-2); INSERT INTO item_state VALUES(2,0,1,1,1);
        INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;").unwrap();
    let referenced = fixture.read(false).unwrap();
    let own = referenced
        .tracks()
        .iter()
        .find(|track| track.sync_id == u64::MAX)
        .unwrap();
    assert!(PendingDelete::prepare(&referenced, own, DEST, UNIX_EPOCH).is_err());
}

#[test]
fn pending_delete_recheck_binds_full_catalogue_bytes_and_anchor() {
    let before = catalog("");
    let deletion = prepare(&before).unwrap();
    for sql in [
        "UPDATE item_extra SET title='Changed';",
        "UPDATE item_stats SET play_count_recent=1;",
        "UPDATE album_artist SET album_artist='Changed orphan' WHERE album_artist_pid=22;",
        "UPDATE _MLDatabaseProperties SET value='141';",
    ] {
        let changed = catalog(sql);
        assert!(deletion.ensure_current(&changed).is_err(), "{sql}");
    }
}

#[test]
fn pending_delete_exhausted_anchor_refuses_before_encoding() {
    let catalog = modified_collection_catalog(u32::MAX, PENDING);
    assert!(prepare(&catalog).is_err());
}
