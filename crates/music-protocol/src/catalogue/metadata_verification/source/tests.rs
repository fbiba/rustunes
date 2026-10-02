use super::*;
use crate::catalogue::metadata_update::{SourceFieldRequest::*, prepare_source_metadata};
use plist::Value;
use std::{io::Cursor, time::SystemTime};

// Actual bounded SQL-reader fixtures, with independent literal poststate SQL.
// The sibling shares the original composer and keeps a SQL NULL title sort.
const BASE: &str = "
    INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
    INSERT INTO item_extra SELECT 2,'Sibling','SIBLING.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,NULL,comment FROM item_extra WHERE item_pid=1;
    INSERT INTO item_store VALUES(2,123);
    INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;
    INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;
    ALTER TABLE item_stats ADD COLUMN date_played REAL;
    ALTER TABLE item_stats ADD COLUMN opaque_value;
    UPDATE item_stats SET date_played=123.25,opaque_value=1;
    INSERT INTO artwork VALUES('100',300,6,0,'aa/00000000000000000000000000000000000000');
    INSERT INTO artwork_token VALUES(1,'100',300,1,0,0);
    INSERT INTO artwork_token VALUES(31,'100',300,6,4,0);";
const MIXED: &str = "
    INSERT INTO composer VALUES(72,0,'New Composer','New Composer');
    UPDATE item SET composer_pid=72 WHERE item_pid=1;
    UPDATE item_extra SET title='Changed title',sort_title='Changed sort',year=2025 WHERE item_pid=1;";

fn catalog(anchor: u32, sql: &str) -> Catalog {
    crate::test_support::catalogue::modified_collection_catalog(anchor, &format!("{BASE}{sql}"))
}
fn mixed() -> SourceMetadataPatch {
    SourceMetadataPatch {
        title: Set("Changed title".into()),
        sort_name: Set("Changed sort".into()),
        composer: Set("New Composer".into()),
        year: Set(2025),
        ..Default::default()
    }
}
fn serialize(
    before: &Catalog,
    binding: &SourceMetadataBinding,
    patch: &SourceMetadataPatch,
) -> Result<Vec<u8>> {
    prepare_source_metadata(before, binding, patch, SystemTime::UNIX_EPOCH)
}

#[test]
fn composed_title_composer_year_matches_one_operation_and_exact_projection() {
    let before = catalog(12, "");
    let after = catalog(13, MIXED);
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    let request = mixed();
    let bytes = serialize(&before, &binding, &request).unwrap();
    let wire = Value::from_reader(Cursor::new(bytes)).unwrap();
    let operations = wire.as_dictionary().unwrap()["operations"]
        .as_array()
        .unwrap();
    assert_eq!(operations.len(), 2);
    assert_eq!(
        operations[0].as_dictionary().unwrap()["operation"].as_string(),
        Some("update_db_info")
    );
    let update = operations[1].as_dictionary().unwrap();
    assert_eq!(update["operation"].as_string(), Some("update_track"));
    assert_eq!(update["pid"].as_signed_integer(), Some(-1));
    let item = update["item"].as_dictionary().unwrap();
    assert_eq!(item.len(), 5);
    assert_eq!(item["title"].as_string(), Some("Changed title"));
    assert_eq!(item["sort_name"].as_string(), Some("Changed sort"));
    assert_eq!(item["composer"].as_string(), Some("New Composer"));
    assert_eq!(item["year"].as_unsigned_integer(), Some(2025));
    assert_eq!(item["album_pid"].as_signed_integer(), Some(-31));
    let receipt = verify_source_metadata(&before, &after, &binding, &request).unwrap();
    assert_eq!(receipt.schema, "music-protocol.source-metadata-projection");
    assert_eq!(receipt.target_track_id, "ffffffffffffffff");
    assert_eq!(receipt.unchanged_album_id, "ffffffffffffffe1");
    assert_eq!(receipt.media_anchor, 13);
    assert_eq!(receipt.requested, request);
    assert_eq!(
        receipt.changed_fields,
        ["composer", "year", "title", "sort_name"]
    );
    assert_eq!(receipt.total_tracks, 2);
    assert_eq!(receipt.other_tracks_preserved, 1);
    assert_eq!(receipt.other_title_sorts_preserved, 1);
    assert_eq!(after.title_sort(123), Some(&None));
    assert!(receipt.all_track_statistics_preserved);
    let json = serde_json::to_value(receipt).unwrap();
    for flag in [
        "raw_sql_verified",
        "permanent_bytes_verified",
        "protocol_terminal_verified",
        "journal_completion_authorized",
    ] {
        assert_eq!(json[flag], false, "{flag}");
    }
    assert_eq!(json["receiver_qualification_required"], true);
    assert_eq!(json["unverified_fields"].as_array().unwrap().len(), 4);
}

#[test]
fn composed_all_supported_scalars_and_title_pair_use_the_same_poststate() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    let request = SourceMetadataPatch {
        genre: Set("New Genre".into()),
        disc: Set(2),
        disc_total: Set(3),
        track: Set(7),
        track_total: Set(11),
        ..mixed()
    };
    let after = catalog(
        13,
        &format!(
            "{MIXED}
        INSERT INTO genre VALUES(62,0,'New Genre');
        UPDATE item SET genre_id=62,disc_number=2,track_number=7 WHERE item_pid=1;
        UPDATE item_extra SET disc_count=3,track_count=11 WHERE item_pid=1;"
        ),
    );
    serialize(&before, &binding, &request).unwrap();
    let receipt = verify_source_metadata(&before, &after, &binding, &request).unwrap();
    assert_eq!(receipt.changed_fields.len(), 9);
    for undo in [
        "UPDATE item SET genre_id=61 WHERE item_pid=1;",
        "UPDATE item SET disc_number=1 WHERE item_pid=1;",
        "UPDATE item SET track_number=6 WHERE item_pid=1;",
        "UPDATE item_extra SET disc_count=0 WHERE item_pid=1;",
        "UPDATE item_extra SET track_count=10 WHERE item_pid=1;",
    ] {
        let wrong = catalog(
            13,
            &format!(
                "{MIXED}
            INSERT INTO genre VALUES(62,0,'New Genre');
            UPDATE item SET genre_id=62,disc_number=2,track_number=7 WHERE item_pid=1;
            UPDATE item_extra SET disc_count=3,track_count=11 WHERE item_pid=1;{undo}"
            ),
        );
        assert!(
            verify_source_metadata(&before, &wrong, &binding, &request).is_err(),
            "{undo}"
        );
    }
}

#[test]
fn composed_rejects_each_omitted_requested_field_and_wrong_target_value() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for undo in [
        "UPDATE item_extra SET title='Synthetic Title' WHERE item_pid=1;",
        "UPDATE item_extra SET sort_title='Synthetic Title' WHERE item_pid=1;",
        "UPDATE item_extra SET sort_title=NULL WHERE item_pid=1;",
        "UPDATE item_extra SET year=2026 WHERE item_pid=1;",
        "UPDATE item SET composer_pid=71 WHERE item_pid=1;",
        "UPDATE item_extra SET title='Wrong title' WHERE item_pid=1;",
        "UPDATE item_extra SET year=2024 WHERE item_pid=1;",
    ] {
        let after = catalog(13, &format!("{MIXED}{undo}"));
        assert!(
            verify_source_metadata(&before, &after, &binding, &mixed()).is_err(),
            "{undo}"
        );
    }
}

#[test]
fn composed_rejects_unrequested_target_and_sibling_changes() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for extra in [
        "UPDATE item_extra SET copyright='Unexpected' WHERE item_pid=1;",
        "UPDATE item SET track_number=7 WHERE item_pid=1;",
        "UPDATE item_extra SET location='OTHER.m4a' WHERE item_pid=1;",
        "UPDATE item_extra SET file_size=file_size+1 WHERE item_pid=1;",
        "UPDATE item_extra SET total_time_ms=total_time_ms+1 WHERE item_pid=1;",
        "UPDATE item_state SET is_placeholder=1 WHERE item_pid=1;",
        "UPDATE item_extra SET title='Unexpected sibling' WHERE item_pid=2;",
        "UPDATE item_extra SET sort_title='' WHERE item_pid=2;",
        "UPDATE item_extra SET sort_title='Unexpected sort' WHERE item_pid=2;",
        "UPDATE item SET composer_order=101 WHERE item_pid=2;",
        "INSERT INTO composer VALUES(73,0,'Synthetic Composer','Synthetic Composer'); UPDATE item SET composer_pid=73 WHERE item_pid=2;",
        "UPDATE item_store SET sync_id=456 WHERE item_pid=2;",
    ] {
        let after = catalog(13, &format!("{MIXED}{extra}"));
        assert!(
            verify_source_metadata(&before, &after, &binding, &mixed()).is_err(),
            "{extra}"
        );
    }
}

#[test]
fn composed_rejects_projected_and_unprojected_typed_statistics_changes() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for extra in [
        "UPDATE item_stats SET play_count_recent=1 WHERE item_pid=1;",
        "UPDATE item_stats SET play_count_user=1 WHERE item_pid=2;",
        "UPDATE item_stats SET skip_count_recent=1 WHERE item_pid=1;",
        "UPDATE item_stats SET user_rating=20 WHERE item_pid=1;",
        "UPDATE item_stats SET date_played=123.5 WHERE item_pid=1;",
        "UPDATE item_stats SET date_played=NULL WHERE item_pid=2;",
        "UPDATE item_stats SET opaque_value=1.0 WHERE item_pid=1;",
        "UPDATE item_stats SET opaque_value='1' WHERE item_pid=2;",
    ] {
        let after = catalog(13, &format!("{MIXED}{extra}"));
        assert!(
            verify_source_metadata(&before, &after, &binding, &mixed()).is_err(),
            "{extra}"
        );
    }
}

#[test]
fn composed_rejects_album_artist_and_artwork_changes() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for extra in [
        "UPDATE album SET album='Changed orphan' WHERE album_pid=32;",
        "UPDATE album SET all_compilations=1 WHERE album_pid=31;",
        "UPDATE album_artist SET album_artist='Changed orphan' WHERE album_artist_pid=22;",
        "UPDATE artwork SET relative_path='bb/00000000000000000000000000000000000000';",
        "DELETE FROM artwork_token WHERE entity_type=4;",
        "INSERT INTO artwork VALUES('101',300,6,0,'aa/00000000000000000000000000000000000000');",
    ] {
        let after = catalog(13, &format!("{MIXED}{extra}"));
        assert!(
            verify_source_metadata(&before, &after, &binding, &mixed()).is_err(),
            "{extra}"
        );
    }
}

#[test]
fn composed_rejects_wrong_anchor_identity_and_stale_binding() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for anchor in [12, 14, u32::MAX] {
        assert!(
            verify_source_metadata(&before, &catalog(anchor, MIXED), &binding, &mixed()).is_err()
        );
    }
    for sql in [
        "UPDATE db_info SET db_pid=-100;",
        "UPDATE db_info SET primary_container_pid=-100;",
    ] {
        let after = catalog(13, &format!("{MIXED}{sql}"));
        assert!(verify_source_metadata(&before, &after, &binding, &mixed()).is_err());
    }
    for sql in [
        "UPDATE item_extra SET sort_title='Stale' WHERE item_pid=1;",
        "UPDATE item_stats SET play_count_user=1 WHERE item_pid=1;",
        "UPDATE item_extra SET year=2000 WHERE item_pid=2;",
    ] {
        let stale = catalog(12, sql);
        assert!(serialize(&stale, &binding, &mixed()).is_err());
        assert!(verify_source_metadata(&stale, &catalog(13, MIXED), &binding, &mixed()).is_err());
    }
}

#[test]
fn composed_rejects_unsupported_clear_invalid_and_empty_requests_atomically() {
    let before = catalog(12, "");
    let after = catalog(13, MIXED);
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for request in [
        SourceMetadataPatch {
            artist: Set("Other artist".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            album: Set("Other album".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            grouping_artist: Set("Other group".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            copyright: Set("Copyright".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            compilation: Set(true),
            ..mixed()
        },
        SourceMetadataPatch {
            sample_rate: Set(48000),
            ..mixed()
        },
        SourceMetadataPatch {
            title: Clear,
            ..mixed()
        },
        SourceMetadataPatch {
            composer: Clear,
            ..mixed()
        },
        SourceMetadataPatch {
            year: Clear,
            ..mixed()
        },
        SourceMetadataPatch {
            year: Set(0),
            ..mixed()
        },
        SourceMetadataPatch {
            year: Set(65536),
            ..mixed()
        },
        SourceMetadataPatch {
            title: Set("".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            composer: Set("".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            composer: Set("Synthetic Composer".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            composer: Set("SYNTHETIC COMPOSER".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            track: Set(11),
            ..mixed()
        },
        SourceMetadataPatch::default(),
    ] {
        assert!(
            serialize(&before, &binding, &request).is_err(),
            "{request:?}"
        );
        assert!(
            verify_source_metadata(&before, &after, &binding, &request).is_err(),
            "{request:?}"
        );
    }
    assert_eq!(before.persisted_media_anchor(), 12);
    assert_eq!(before.tracks()[0].metadata.title, "Synthetic Title");
}

#[test]
fn composed_preserve_title_or_sort_uses_exact_observed_counterpart() {
    let before = catalog(12, "");
    let binding = metadata_update::bind_source_metadata(&before, u64::MAX).unwrap();
    for (request, sql, wrong) in [
        (
            SourceMetadataPatch {
                title: Set("Changed title".into()),
                ..Default::default()
            },
            "UPDATE item_extra SET title='Changed title' WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title='Changed title' WHERE item_pid=1;",
        ),
        (
            SourceMetadataPatch {
                sort_name: Set("Changed sort".into()),
                ..Default::default()
            },
            "UPDATE item_extra SET sort_title='Changed sort' WHERE item_pid=1;",
            "UPDATE item_extra SET title='Changed sort' WHERE item_pid=1;",
        ),
        (
            SourceMetadataPatch {
                year: Set(2025),
                ..Default::default()
            },
            "UPDATE item_extra SET year=2025 WHERE item_pid=1;",
            "UPDATE item_extra SET sort_title=NULL WHERE item_pid=1;",
        ),
    ] {
        serialize(&before, &binding, &request).unwrap();
        let receipt =
            verify_source_metadata(&before, &catalog(13, sql), &binding, &request).unwrap();
        assert_eq!(receipt.changed_fields.len(), 1);
        assert!(
            verify_source_metadata(
                &before,
                &catalog(13, &format!("{sql}{wrong}")),
                &binding,
                &request
            )
            .is_err()
        );
        let unrequested_composer = catalog(
            13,
            &format!("{sql}UPDATE composer SET sort_composer='Unexpected';"),
        );
        assert!(
            verify_source_metadata(&before, &unrequested_composer, &binding, &request).is_err()
        );
    }
}
