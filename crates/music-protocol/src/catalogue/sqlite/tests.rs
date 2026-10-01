use super::*;
use crate::test_support::catalogue::{
    Fixture, collection_catalog, composer_transition_catalog, modified_collection_catalog,
};
#[test]
fn catalog_sqlite_controlled_empty_query_checks_effective_policy_after_sql() {
    let fixture = Fixture::new(false);
    for expired in [false, true] {
        let connection = open_readonly(&fixture.database).unwrap();
        let policy = ReadControl {
            deadline: Instant::now() + Duration::from_millis(100),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let reached = Arc::new(AtomicBool::new(false));
        let marker = reached.clone();
        let flag = policy.cancelled.clone();
        let deadline = policy.deadline;
        // Let the real empty query finish: the Rust post-query check must refuse.
        connection
            .progress_handler(
                1,
                Some(move || {
                    marker.store(true, Ordering::Relaxed);
                    if expired {
                        while Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    } else {
                        flag.store(true, Ordering::Relaxed);
                    }
                    false
                }),
            )
            .unwrap();
        let mut statement = connection.prepare("SELECT 1 WHERE 0").unwrap();
        let mut rows = statement.query([]).unwrap();
        assert!(next_row(&mut rows, Some(&policy)).is_err());
        assert!(
            reached.load(Ordering::Relaxed),
            "query never reached SQLite"
        );
    }
}
#[test]
fn catalog_sqlite_controlled_catalog_and_wal_guards_match_legacy() {
    for has_wal in [false, true] {
        let fixture = Fixture::new(has_wal);
        let wal = fixture.database.with_file_name("fixture.db-wal");
        let old = fixture.read(has_wal).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let new = read_snapshot_with_control(
            &fixture.database,
            has_wal.then_some(wal.as_path()),
            &fixture.directory.0,
            Instant::now() + Duration::from_secs(30),
            flag.clone(),
        )
        .unwrap();
        assert_eq!(old.tracks(), new.tracks());
        assert_eq!(old.source_hashes().database, new.source_hashes().database);
        assert_eq!(old.source_hashes().wal, new.source_hashes().wal);
        flag.store(true, Ordering::Relaxed);
        assert!(
            read_snapshot_with_control(
                &fixture.database,
                has_wal.then_some(wal.as_path()),
                &fixture.directory.0,
                Instant::now() + Duration::from_secs(30),
                flag
            )
            .is_err()
        );
    }
    let (fixture, mut bytes, _) = reset_wal_fixture();
    let wal = fixture.directory.0.join("controlled-reset-wal");
    fs::write(&wal, &bytes).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let flag = Arc::new(AtomicBool::new(false));
    validate_container_with_control(&fixture.database, Some(&wal), deadline, flag.clone()).unwrap();
    bytes[32 + 24] ^= 1;
    fs::write(&wal, &bytes).unwrap();
    assert!(
        validate_container_with_control(&fixture.database, Some(&wal), deadline, flag.clone())
            .unwrap_err()
            .to_string()
            .contains("WAL frame checksum differs")
    );
    fs::write(&wal, []).unwrap();
    validate_container_with_control(&fixture.database, Some(&wal), deadline, flag.clone()).unwrap();
    for wal_path in [None, Some(wal.as_path())] {
        flag.store(true, Ordering::Relaxed);
        assert!(
            validate_container_with_control(&fixture.database, wal_path, deadline, flag.clone())
                .is_err()
        );
        flag.store(false, Ordering::Relaxed);
        assert!(
            validate_container_with_control(
                &fixture.database,
                wal_path,
                Instant::now() - Duration::from_secs(1),
                flag.clone()
            )
            .is_err()
        );
    }
}

#[test]
fn catalog_sqlite_controlled_progress_observes_later_flag_and_original_deadline() {
    // A generated storage fixture proves SQL interruption, not receiver effects.
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch("CREATE TABLE progress_fixture(value INTEGER);")
        .unwrap();
    for value in 0..256 {
        fixture
            .connection
            .execute("INSERT INTO progress_fixture VALUES(?1)", [value])
            .unwrap();
    }
    const COUNT: &str = "SELECT count(*) FROM progress_fixture a CROSS JOIN progress_fixture b";
    for expired in [false, true] {
        let control = ReadControl {
            deadline: Instant::now() + Duration::from_millis(100),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (connection, _) = open_readonly_with_control(&fixture.database, &control).unwrap();
        assert_eq!(
            connection
                .query_row(COUNT, [], |r| r.get::<_, i64>(0))
                .unwrap(),
            65_536
        );
        if expired {
            while Instant::now() < control.deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        } else {
            control.cancelled.store(true, Ordering::Relaxed);
        }
        let result = connection.query_row(COUNT, [], |r| r.get::<_, i64>(0));
        assert!(
            matches!(result, Err(rusqlite::Error::SqliteFailure(ref error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted),
            "SQL progress accepted stopped policy: {result:?}"
        );
    }
}

#[test]
fn raw_stats_fingerprint_excludes_only_recent_play_and_preserves_dates_and_storage_types() {
    let catalog = |recent: u32, date: &str, opaque: &str| {
        modified_collection_catalog(
            12,
            &format!(
                "ALTER TABLE item_stats ADD COLUMN date_played REAL; ALTER TABLE item_stats ADD COLUMN opaque_value; \
         UPDATE item_stats SET play_count_recent={recent},date_played={date},opaque_value={opaque};"
            ),
        )
    };
    let initial = catalog(3, "123.25", "1");
    let reset = catalog(0, "123.25", "1");
    assert_eq!(
        initial.stats_except_recent_play(),
        reset.stats_except_recent_play()
    );
    for changed in [
        catalog(0, "123.5", "1"),
        catalog(0, "123.25", "1.0"),
        catalog(0, "123.25", "'1'"),
        catalog(0, "123.25", "x'31'"),
        catalog(0, "NULL", "1"),
    ] {
        assert_ne!(
            initial.stats_except_recent_play(),
            changed.stats_except_recent_play()
        );
    }
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch("UPDATE item_stats SET play_count_recent=-1")
        .unwrap();
    assert!(fixture.read(false).is_err());
}

#[test]
fn composer_lookup_sort_and_order_are_typed_and_ambiguity_refuses() {
    let catalog = composer_transition_catalog("ACME", Some("Different sort"), 123, false);
    let row = catalog.composer_for_label("Acme").unwrap().unwrap();
    assert_eq!(row.name, "ACME");
    assert_eq!(row.sort_name.as_deref(), Some("Different sort"));
    assert_eq!(
        catalog.track_composers()[&u64::MAX].lookup_id,
        Some(row.internal_id)
    );
    assert_eq!(catalog.track_composers()[&u64::MAX].order, 123);
    assert!(catalog.composer_for_label("Other").unwrap().is_none());
    for sql in [
        "UPDATE item SET composer_order=NULL",
        "UPDATE item SET composer_order='bad'",
        "UPDATE composer SET sort_composer=x'00'",
        "UPDATE composer SET composer_pid=0",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch("INSERT INTO composer VALUES(72,0,'SYNTHETIC COMPOSER','other sort')")
        .unwrap();
    assert!(
        fixture
            .read(false)
            .unwrap()
            .composer_for_label("Synthetic Composer")
            .is_err()
    );
    let unicode = composer_transition_catalog("ÉCHO", Some("ÉCHO"), 0, false);
    assert!(unicode.composer_for_label("écho").unwrap().is_none());
    assert_eq!(
        unicode.composer_for_label("ÉCHO").unwrap().unwrap().name,
        "ÉCHO"
    );
}

#[test]
fn title_sort_preserves_null_empty_and_exact_text_without_changing_track_projection() {
    let initial = collection_catalog(12);
    let projected = initial.tracks().to_vec();
    assert_eq!(
        initial.title_sort(u64::MAX),
        Some(&Some("Synthetic Title".into()))
    );
    assert!(initial.title_sort(123).is_none());
    for (sql, expected) in [
        ("NULL", None),
        ("''", Some(String::new())),
        ("'Σύνολο; Exact Sort'", Some("Σύνολο; Exact Sort".into())),
    ] {
        let changed =
            modified_collection_catalog(12, &format!("UPDATE item_extra SET sort_title={sql}"));
        assert_eq!(changed.title_sort(u64::MAX), Some(&expected));
        assert_eq!(changed.tracks(), projected);
    }
}

#[test]
fn title_sort_rejects_missing_column_and_invalid_sql_storage() {
    for sql in [
        "ALTER TABLE item_extra DROP COLUMN sort_title",
        "UPDATE item_extra SET sort_title=x'00'",
        "UPDATE item_extra SET sort_title=CAST(x'ff' AS TEXT)",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn compilation_summary_and_live_flags_are_independent_and_strict() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch("UPDATE item SET is_compilation=1")
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    let album = &catalog.albums()[&31];
    assert!(!album.all_compilations);
    assert!(album.has_compilation_tracks);
    assert!(catalog.tracks()[0].metadata.is_compilation);
    fixture
        .connection
        .execute_batch("UPDATE album SET all_compilations=1 WHERE album_pid=31")
        .unwrap();
    assert!(fixture.read(false).unwrap().albums()[&31].all_compilations);
    fixture
        .connection
        .execute_batch("UPDATE item SET is_compilation=2")
        .unwrap();
    assert!(fixture.read(false).is_err());
    fixture
        .connection
        .execute_batch("UPDATE item SET is_compilation=0; UPDATE album SET all_compilations=-1")
        .unwrap();
    assert!(fixture.read(false).is_err());
}

#[test]
fn rating_compilation_and_playback_values_are_preserved_separately() {
    let fixture = Fixture::new(true);
    fixture.connection.execute_batch("UPDATE item SET is_compilation=1;
        UPDATE item_extra SET content_rating=1,content_rating_level=42;
        UPDATE item_stats SET play_count_user=20,play_count_recent=3,skip_count_user=9,skip_count_recent=2,
            has_been_played=1,user_rating=80,user_rating_is_derived=1,liked_state=2;").unwrap();
    let catalog = fixture.read(true).unwrap();
    let track = &catalog.tracks()[0];
    assert_eq!(track.metadata.content_rating, 1);
    // Preserve unknown numeric codes instead of assigning an enum meaning.
    assert_eq!(track.metadata.content_rating_level, 42);
    assert!(track.metadata.is_compilation);
    assert_eq!(
        track.playback,
        PlaybackStatistics {
            play_count_user: 20,
            play_count_recent: 3,
            skip_count_user: 9,
            skip_count_recent: 2,
            has_been_played: true,
            user_rating: 80,
            user_rating_is_derived: true,
            liked_state: 2,
        }
    );
    // Recent counts may be consumed by synchronization without resetting user counts.
    fixture
        .connection
        .execute_batch("UPDATE item_stats SET play_count_recent=0,skip_count_recent=0")
        .unwrap();
    let after = fixture.read(true).unwrap();
    assert_eq!(after.tracks()[0].playback.play_count_user, 20);
    assert_eq!(after.tracks()[0].playback.skip_count_user, 9);
    assert_eq!(after.tracks()[0].playback.play_count_recent, 0);
    assert_eq!(after.tracks()[0].playback.skip_count_recent, 0);
}

#[test]
fn rating_and_playback_numbers_require_bounded_integer_storage() {
    let fixture = Fixture::new(false);
    for (table, column) in [
        ("item_extra", "content_rating"),
        ("item_extra", "content_rating_level"),
        ("item_stats", "play_count_user"),
        ("item_stats", "play_count_recent"),
        ("item_stats", "skip_count_user"),
        ("item_stats", "skip_count_recent"),
        ("item_stats", "user_rating"),
        ("item_stats", "liked_state"),
    ] {
        for value in ["-1", "4294967296", "0.5", "NULL", "'invalid'", "x'01'"] {
            let sql = format!("UPDATE {table} SET {column}={value}");
            fixture.connection.execute_batch(&sql).unwrap();
            assert!(fixture.read(false).is_err(), "{sql}");
            fixture
                .connection
                .execute_batch(&format!("UPDATE {table} SET {column}=0"))
                .unwrap();
        }
    }
    for column in ["has_been_played", "user_rating_is_derived"] {
        for value in ["-1", "2", "0.5", "NULL", "'invalid'"] {
            let sql = format!("UPDATE item_stats SET {column}={value}");
            fixture.connection.execute_batch(&sql).unwrap();
            assert!(fixture.read(false).is_err(), "{sql}");
            fixture
                .connection
                .execute_batch(&format!("UPDATE item_stats SET {column}=0"))
                .unwrap();
        }
    }
    assert_eq!(
        fixture.read(false).unwrap().tracks()[0].playback,
        PlaybackStatistics::default()
    );
}

#[test]
fn wal_only_committed_row_and_orphans_are_preserved_without_source_changes() {
    let fixture = Fixture::new(true);
    let wal = fixture.database.with_file_name("fixture.db-wal");
    let before = (
        hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
        hash_file(&wal, MAX_WAL_BYTES).unwrap(),
    );
    let catalog = fixture.read(true).unwrap();
    assert_eq!(catalog.tracks().len(), 1);
    assert_eq!(catalog.tracks()[0].sync_id, u64::MAX);
    assert_eq!(catalog.tracks()[0].metadata.total_time_ms, 147004);
    assert_eq!(
        catalog.music_paths().unwrap(),
        BTreeSet::from(["iTunes_Control/Music/F39/TEST.m4a".to_owned()])
    );
    assert_eq!(catalog.albums()[&32].live_track_count, 0);
    assert_eq!(catalog.artists()[&22].live_track_count, 0);
    assert!(catalog.represented_sync_ids().contains(&(-32i64 as u64)));
    assert_eq!(catalog.persisted_media_anchor(), 12);
    assert_eq!(
        before,
        (
            hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
            hash_file(&wal, MAX_WAL_BYTES).unwrap()
        )
    );
    assert!(!fs::read_dir(&fixture.directory.0).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("catalog-sqlite-")
    }));
}

#[test]
fn absent_and_empty_wal_are_distinguished() {
    let fixture = Fixture::new(false);
    let catalog = fixture.read(false).unwrap();
    assert!(catalog.source_hashes().wal.is_none());
    let wal = fixture.database.with_file_name("fixture.db-wal");
    fs::write(&wal, []).unwrap();
    let catalog = fixture.read(true).unwrap();
    assert_eq!(catalog.source_hashes().wal_bytes, Some(0));
    assert!(catalog.source_hashes().wal.is_some());
}

#[test]
fn schema_missing_columns_and_wrong_versions_fail() {
    for sql in [
        "PRAGMA user_version=1",
        "PRAGMA user_version=2400059",
        "PRAGMA user_version=2410000; ALTER TABLE item_extra RENAME COLUMN title TO absent",
        "DROP TABLE item_stats",
        "ALTER TABLE item_extra RENAME COLUMN content_rating TO absent",
        "ALTER TABLE item_stats RENAME COLUMN play_count_user TO absent",
        "ALTER TABLE item_extra RENAME COLUMN title TO absent",
        "ALTER TABLE item_extra RENAME TO old_extra; CREATE VIEW item_extra AS SELECT * FROM old_extra",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn newer_version_with_the_read_tables_is_read_as_untested() {
    let fixture = Fixture::new(false);
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.user_version(), 2_400_060);
    assert!(catalog.user_version_tested());
    // An index-only version bump, like iOS 27.2's.
    fixture
        .connection
        .execute_batch(
            "PRAGMA user_version=2410000; CREATE INDEX AlbumCloudChannelName ON album (sort_album ASC);",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.user_version(), 2_410_000);
    assert!(catalog.user_version_tested());
    fixture
        .connection
        .execute_batch("PRAGMA user_version=2420000")
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.user_version(), 2_420_000);
    assert!(!catalog.user_version_tested());
    assert_eq!(catalog.tracks().len(), 1);
}

#[test]
fn missing_or_conflicting_joins_fail() {
    for sql in [
        "DELETE FROM item_store",
        "DELETE FROM item_stats",
        "UPDATE item_stats SET item_pid=99",
        "UPDATE item_store SET item_pid=99",
        "UPDATE item SET album_pid=999",
        "UPDATE item SET album_artist_pid=22",
        "UPDATE item SET item_artist_pid=999",
        "UPDATE item SET genre_id=999",
        "UPDATE item SET composer_pid=999",
        "UPDATE item SET base_location_id=999",
        "UPDATE album SET album_artist_pid=999 WHERE album_pid=32",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn zero_or_duplicate_wire_ids_fail() {
    for sql in [
        // A track with a file, its album or its album artist without a sync id.
        "UPDATE item_store SET sync_id=0",
        "UPDATE album SET sync_id=0 WHERE album_pid=31",
        "UPDATE album_artist SET sync_id=0 WHERE album_artist_pid=21",
        // An album with a sync id under an album artist without one.
        "UPDATE album_artist SET sync_id=0 WHERE album_artist_pid=22",
        "UPDATE db_info SET db_pid=0",
        "UPDATE album SET sync_id=-31 WHERE album_pid=32",
        "UPDATE album_artist SET sync_id=-21 WHERE album_artist_pid=22",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

/// The observed shape of an entry the phone adds outside the library: an
/// Apple Music track it streamed, with a remote location and no file, under an
/// album and album artist of its own, none of the three with a sync id. Its
/// cover is a store image (source 600); the album also has a source300 one.
const STREAMED_ENTRY: &str = "INSERT INTO album_artist VALUES(23,0,'Streamed Artist',NULL);
    INSERT INTO album VALUES(33,0,'Streamed Album',23,0,NULL);
    INSERT INTO item_artist VALUES(42,0,'Streamed Artist',NULL);
    INSERT INTO item VALUES(2,8,33,23,42,61,0,0,50,3,1,0,0);
    INSERT INTO item_extra VALUES(2,'Streamed Title','',214000.0,0,0,0,0,NULL,0,0,NULL,NULL);
    INSERT INTO item_store VALUES(2,0);
    INSERT INTO item_state VALUES(2,0,0,0,1);
    INSERT INTO item_stats VALUES(2,0,0,0,0,0,0,0,0);
    INSERT INTO artwork_token VALUES(2,'https://example.invalid/cover.jpg',600,1,0,0);
    INSERT INTO artwork_token VALUES(33,'87654',300,6,4,0);";

#[test]
fn an_entry_the_phone_added_outside_the_library_is_left_out() {
    let fixture = Fixture::new(false);
    let before = fixture.read(false).unwrap();
    fixture.connection.execute_batch(STREAMED_ENTRY).unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.tracks(), before.tracks());
    let keys = |c: &Catalog| {
        (
            c.albums().keys().copied().collect::<Vec<_>>(),
            c.artists().keys().copied().collect::<Vec<_>>(),
        )
    };
    assert_eq!(keys(&catalog), keys(&before));
    let album = &catalog.unsynced_albums()[&33];
    assert_eq!(
        (album.sync_id, album.name.as_str(), album.artist_sync_id),
        (0, "Streamed Album", 0)
    );
    let artist = &catalog.unsynced_artists()[&23];
    assert_eq!(
        (artist.sync_id, artist.name.as_str()),
        (0, "Streamed Artist")
    );
    assert_eq!(catalog.unsynced_albums().len(), 1);
    assert_eq!(catalog.unsynced_artists().len(), 1);
    assert_eq!(
        catalog.represented_sync_ids(),
        before.represented_sync_ids()
    );
    assert_eq!(
        catalog.stats_except_recent_play(),
        before.stats_except_recent_play()
    );
    assert_eq!(catalog.playback_dates(), before.playback_dates());
    assert_eq!(catalog.title_sort(0), None);
    assert!(catalog.item_artists().contains_key(&42));
    let artwork = catalog.artwork();
    assert!(artwork.tracks_with_artwork_registration().is_empty());
    assert!(artwork.occupied_cache_ids().contains(&87654));
    assert!(artwork.album_cache_ids().is_empty() && artwork.orphan_album_cache_ids().is_empty());
    assert_eq!(artwork.unmapped_source300_token_rows(), 1);
    assert_eq!(artwork.other_source_rows()[&600].registration_rows, 1);
    assert_eq!(catalog.music_paths().unwrap().len(), 1);

    // Filed under the library's own groups, it leaves their member counts alone.
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch(STREAMED_ENTRY).unwrap();
    fixture
        .connection
        .execute_batch("UPDATE item SET album_pid=31,album_artist_pid=21 WHERE item_pid=2")
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.tracks(), before.tracks());
    assert_eq!(catalog.albums()[&31].live_track_count, 1);
    assert_eq!(catalog.artists()[&21].live_track_count, 1);
}

#[test]
fn an_entry_the_phone_added_with_a_file_or_shared_groups_refuses() {
    for sql in [
        "UPDATE item SET base_location_id=51 WHERE item_pid=2",
        "UPDATE item_extra SET location='STREAM.m4a' WHERE item_pid=2",
        "UPDATE item_state SET has_non_purgeable_asset=1 WHERE item_pid=2",
        "UPDATE item SET album_pid=33,album_artist_pid=23 WHERE item_pid=1",
        "UPDATE album SET album_artist_pid=23 WHERE album_pid=32",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(STREAMED_ENTRY).unwrap();
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn duplicate_track_sync_ids_and_casefold_path_aliases_fail() {
    for same_id in [true, false] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch("INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
            INSERT INTO item_extra SELECT 2,title,'test.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;
            INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;
            INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;").unwrap();
        fixture
            .connection
            .execute(
                "INSERT INTO item_store VALUES(2,?1)",
                [if same_id { -1i64 } else { -2i64 }],
            )
            .unwrap();
        assert!(fixture.read(false).is_err());
    }
}

#[test]
fn repeated_collection_names_count_against_aggregate_text_budget() {
    let fixture = Fixture::new(false);
    let name = "x".repeat(MAX_TEXT_BYTES);
    fixture
        .connection
        .execute("UPDATE album SET album=?1 WHERE album_pid=31", [&name])
        .unwrap();
    fixture
        .connection
        .execute(
            "UPDATE album_artist SET album_artist=?1 WHERE album_artist_pid=21",
            [&name],
        )
        .unwrap();
    fixture.connection.execute_batch("BEGIN").unwrap();
    for item in 2..40 {
        fixture.connection.execute("INSERT INTO item SELECT ?1,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1",[item]).unwrap();
        fixture.connection.execute("INSERT INTO item_extra SELECT ?1,title,?2,total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1",rusqlite::params![item,format!("T{item}.m4a")]).unwrap();
        fixture
            .connection
            .execute("INSERT INTO item_store VALUES(?1,?2)", [item, -item])
            .unwrap();
        fixture.connection.execute("INSERT INTO item_state SELECT ?1,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1",[item]).unwrap();
        fixture.connection.execute("INSERT INTO item_stats SELECT ?1,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1",[item]).unwrap();
    }
    fixture.connection.execute_batch("COMMIT").unwrap();
    let error = fixture
        .read(false)
        .err()
        .expect("aggregate clone budget must reject");
    assert!(error.to_string().contains("aggregate catalog text"));
}

#[test]
fn invalid_numeric_storage_and_values_fail() {
    for sql in [
        "UPDATE item_store SET sync_id='not an integer'",
        "UPDATE item_extra SET total_time_ms=1.5",
        "UPDATE item_extra SET total_time_ms=-1",
        "UPDATE item_extra SET file_size=-1",
        "UPDATE item SET track_number=4294967296",
        "UPDATE item_state SET is_placeholder=2",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn anchor_is_required_and_canonical() {
    for value in ["", "012", "-1", "12x", "4294967296"] {
        let fixture = Fixture::new(false);
        fixture
            .connection
            .execute("UPDATE _MLDatabaseProperties SET value=?1", [value])
            .unwrap();
        assert!(fixture.read(false).is_err());
    }
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute("DELETE FROM _MLDatabaseProperties", [])
        .unwrap();
    assert!(fixture.read(false).is_err());
}

#[test]
fn unsafe_music_paths_fail() {
    for leaf in [
        "../TEST.m4a",
        "a/b.m4a",
        "a\\b.m4a",
        "%2f.m4a",
        "é.m4a",
        ".",
        "..",
        "",
    ] {
        assert!(music_path("iTunes_Control/Music/F39", leaf).is_err());
    }
    for base in [
        "/iTunes_Control/Music/F39",
        "iTunes_Control/Music/F3",
        "iTunes_Control/Music/F３９",
        "iTunes_Control/Other/F39",
    ] {
        assert!(music_path(base, "TEST.m4a").is_err());
    }
}

#[test]
fn missing_location_is_retained_but_collision_set_refuses_it() {
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch("UPDATE item SET base_location_id=0; UPDATE item_extra SET location=''; UPDATE item_state SET has_non_purgeable_asset=0;").unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.tracks().len(), 1);
    assert!(catalog.tracks()[0].afc_path.is_none());
    assert!(catalog.music_paths().is_err());
}

#[test]
fn zero_base_empty_sentinel_retains_pending_track_without_write_capability() {
    for has_wal in [false, true] {
        let fixture = Fixture::new(has_wal);
        let before = fixture.read(has_wal).unwrap();
        // Synthetic fixture matching an observed pending insert's representation:
        // row-zero sentinel exists, both location strings are empty, and no
        // non-purgeable asset is present.
        // Keep the original available row too: parsing must never skip either.
        fixture
            .connection
            .execute_batch(
                "INSERT INTO base_location VALUES(0,'');
                 INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,
                     item_artist_pid,genre_id,composer_pid,0,remote_location_id,
                     track_number,disc_number,is_compilation,composer_order
                     FROM item WHERE item_pid=1;
                 INSERT INTO item_extra SELECT 2,'Pending Title','',total_time_ms,
                     file_size,track_count,disc_count,year,copyright,content_rating,
                     content_rating_level,'Pending Title',comment
                     FROM item_extra WHERE item_pid=1;
                 INSERT INTO item_store VALUES(2,-2);
                 INSERT INTO item_state VALUES(2,0,1,0,1);
                 INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,
                     skip_count_user,skip_count_recent,has_been_played,user_rating,
                     user_rating_is_derived,liked_state
                     FROM item_stats WHERE item_pid=1;",
            )
            .unwrap();
        let catalog = fixture.read(has_wal).unwrap();
        assert_eq!(catalog.tracks().len(), 2);
        let original = catalog
            .tracks()
            .iter()
            .find(|track| track.sync_id == u64::MAX)
            .unwrap();
        assert_eq!(original, &before.tracks()[0]);
        let pending_id = -2_i64 as u64;
        let pending = catalog
            .tracks()
            .iter()
            .find(|track| track.sync_id == pending_id)
            .unwrap();
        assert_eq!(pending.internal_id, 2);
        assert!(pending.afc_path.is_none());
        assert!(!pending.availability.is_placeholder);
        assert!(pending.availability.is_library_added);
        assert!(!pending.availability.has_non_purgeable_asset);
        assert!(pending.availability.is_supported_type);
        assert!(catalog.represented_sync_ids().contains(&pending_id));
        assert!(catalog.music_paths().is_err());
        assert!(
            crate::catalogue::mutation::delete(
                &catalog,
                pending_id,
                std::time::SystemTime::UNIX_EPOCH,
            )
            .is_err()
        );
        assert!(
            crate::catalogue::mutation::title(
                &catalog,
                pending_id,
                "Changed Title",
                "Changed Title",
                std::time::SystemTime::UNIX_EPOCH,
            )
            .is_err()
        );
    }
}

#[test]
fn zero_base_location_refuses_nonempty_or_installed_asset_shapes() {
    for sql in [
        "UPDATE item_extra SET location='TEST.m4a'",
        "UPDATE base_location SET path='iTunes_Control/Music/F39' WHERE base_location_id=0",
        "UPDATE base_location SET path=' ' WHERE base_location_id=0",
        "UPDATE base_location SET path=X'' WHERE base_location_id=0",
        "UPDATE item_state SET has_non_purgeable_asset=1",
        "UPDATE item_extra SET location=NULL",
        "UPDATE item SET base_location_id=51; UPDATE base_location SET path='' WHERE base_location_id=51",
    ] {
        let fixture = Fixture::new(false);
        fixture
            .connection
            .execute_batch(
                "INSERT INTO base_location VALUES(0,'');
                 UPDATE item SET base_location_id=0;
                 UPDATE item_extra SET location='';
                 UPDATE item_state SET has_non_purgeable_asset=0;",
            )
            .unwrap();
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn zero_base_null_sentinel_keeps_existing_locationless_behavior() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch(
            "INSERT INTO base_location VALUES(0,NULL);
             UPDATE item SET base_location_id=0;
             UPDATE item_extra SET location='';
             UPDATE item_state SET has_non_purgeable_asset=0;",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(catalog.tracks().len(), 1);
    assert!(catalog.tracks()[0].afc_path.is_none());
    assert!(catalog.music_paths().is_err());
}

#[test]
fn text_and_input_limits_fail_closed() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute(
            "UPDATE item_extra SET title=?1",
            ["x".repeat(MAX_TEXT_BYTES + 1)],
        )
        .unwrap();
    assert!(fixture.read(false).is_err());
    let oversize = fixture.directory.0.join("oversize.db");
    File::create(&oversize)
        .unwrap()
        .set_len(MAX_DATABASE_BYTES + 1)
        .unwrap();
    assert!(read_snapshot(&oversize, None, &fixture.directory.0).is_err());
    fs::remove_file(oversize).unwrap();
}

#[test]
fn malformed_or_truncated_wal_is_not_silently_ignored() {
    let fixture = Fixture::new(true);
    let journal = fixture.database.with_file_name("fixture.db-wal");
    let original = fs::read(&journal).unwrap();
    for bytes in [
        vec![0; 32],
        original[..original.len() - 1].to_vec(),
        {
            let mut b = original.clone();
            b[24] ^= 1;
            b
        },
        {
            let mut b = original.clone();
            b[60] ^= 1;
            b
        },
    ] {
        let invalid = fixture.directory.0.join("invalid-wal");
        fs::write(&invalid, bytes).unwrap();
        assert!(read_snapshot(&fixture.database, Some(&invalid), &fixture.directory.0).is_err());
        fs::remove_file(invalid).unwrap();
    }
}

/// Produce real SQLite reset reuse: seven new one-page commits overwrite the
/// start of a much larger WAL, leaving the preceding generation's bytes intact.
fn reset_wal_fixture() -> (Fixture, Vec<u8>, usize) {
    let fixture = Fixture::new(true);
    let journal = fixture.database.with_file_name("fixture.db-wal");
    let previous = fs::read(&journal).unwrap();
    fixture
        .connection
        .execute_batch("PRAGMA wal_checkpoint(RESTART);")
        .unwrap();
    for anchor in 13..=19 {
        fixture
            .connection
            .execute(
                "UPDATE _MLDatabaseProperties SET value=?1 WHERE key='MLSyncClientSyncAnchor'",
                [anchor.to_string()],
            )
            .unwrap();
    }
    let bytes = fs::read(journal).unwrap();
    assert_eq!(be32(&bytes, 8), 4096);
    assert_eq!(bytes.len(), previous.len());
    assert_eq!(be32(&previous, 16).wrapping_add(1), be32(&bytes, 16));
    assert_ne!(be32(&previous, 20), be32(&bytes, 20));
    let frame_size = 4096 + 24;
    let current = bytes[32..]
        .chunks_exact(frame_size)
        .take_while(|frame| frame[8..16] == bytes[16..24])
        .count();
    assert_eq!(current, 7);
    let end = 32 + current * frame_size;
    assert!(bytes.len() > 32_768 + frame_size);
    assert_eq!(&bytes[end..], &previous[end..]);
    (fixture, bytes, end)
}

#[test]
fn reset_wal_full_previous_generation_tail_preserves_current_commits_and_sources() {
    let (fixture, bytes, _) = reset_wal_fixture();
    let journal = fixture.database.with_file_name("fixture.db-wal");
    let before_db = hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap();
    let before_wal = hash_file(&journal, MAX_WAL_BYTES).unwrap();
    let catalog = fixture.read(true).unwrap();
    assert_eq!(catalog.persisted_media_anchor(), 19);
    // Discarding the WAL loses the current state even though both views have
    // the same track count and pass SQLite integrity checks.
    let database_only = fixture.read(false).unwrap();
    assert_eq!(database_only.persisted_media_anchor(), 12);
    assert!(catalog.tracks() == database_only.tracks());
    assert_eq!(catalog.source_hashes().wal_bytes, Some(bytes.len() as u64));
    assert_eq!(catalog.source_hashes().wal, Some(before_wal.0));
    assert_eq!(
        hash_file(&fixture.database, MAX_DATABASE_BYTES).unwrap(),
        before_db
    );
    assert_eq!(hash_file(&journal, MAX_WAL_BYTES).unwrap(), before_wal);
}

#[test]
fn reset_wal_observed_32768_byte_shape_and_full_old_header_are_accepted_unchanged() {
    let (fixture, bytes, current_end) = reset_wal_fixture();
    assert_eq!(current_end, 28_872);
    assert_eq!(32_768 - current_end, 3896);
    let copy = fixture.directory.0.join("retained-tail-wal");
    for end in [current_end + 24, 32_768, current_end + 4096 + 24 + 24] {
        fs::write(&copy, &bytes[..end]).unwrap();
        let before = hash_file(&copy, MAX_WAL_BYTES).unwrap();
        let catalog = read_snapshot(&fixture.database, Some(&copy), &fixture.directory.0).unwrap();
        assert_eq!(catalog.persisted_media_anchor(), 19);
        assert_eq!(catalog.tracks().len(), 1);
        assert_eq!(catalog.source_hashes().wal_bytes, Some(end as u64));
        assert_eq!(catalog.source_hashes().wal, Some(before.0));
        assert_eq!(hash_file(&copy, MAX_WAL_BYTES).unwrap(), before);
    }
    fs::remove_file(copy).unwrap();
}

#[test]
fn reset_wal_ambiguous_partial_and_mixed_generation_tails_refuse() {
    let (fixture, bytes, end) = reset_wal_fixture();
    let second_old = end + 4096 + 24;
    let current_salts = bytes[16..24].to_vec();
    let mut cases = vec![
        ("short header", bytes[..end + 23].to_vec()),
        (
            "short header after old frame",
            bytes[..second_old + 23].to_vec(),
        ),
        ("no current prefix", [&bytes[..32], &bytes[end..]].concat()),
    ];
    let mut same_current = bytes[..end + 24].to_vec();
    same_current[end + 8..end + 16].copy_from_slice(&current_salts);
    cases.push(("current-salt partial frame", same_current));
    for (label, salt) in [
        ("older generation", be32(&bytes, 16).wrapping_sub(2)),
        ("future generation", be32(&bytes, 16).wrapping_add(1)),
    ] {
        let mut modified = bytes.clone();
        modified[end + 8..end + 12].copy_from_slice(&salt.to_be_bytes());
        cases.push((label, modified));
    }
    let mut unchanged_salt2 = bytes.clone();
    unchanged_salt2[end + 12..end + 16].copy_from_slice(&bytes[20..24]);
    cases.push(("unchanged random salt", unchanged_salt2));
    let mut mixed = bytes.clone();
    mixed[second_old + 12] ^= 1;
    cases.push(("mixed old generation", mixed));
    let mut reentry = bytes.clone();
    reentry[second_old + 8..second_old + 16].copy_from_slice(&current_salts);
    cases.push(("current generation after old tail", reentry));
    let mut zero_page = bytes.clone();
    zero_page[end..end + 4].fill(0);
    cases.push(("zero old page", zero_page));
    let mut oversized_commit = bytes.clone();
    oversized_commit[end + 4..end + 8].copy_from_slice(&u32::MAX.to_be_bytes());
    cases.push(("oversized old commit", oversized_commit));
    let invalid = fixture.directory.0.join("invalid-reset-wal");
    for (label, data) in cases {
        fs::write(&invalid, data).unwrap();
        assert!(
            validate_container(&fixture.database, Some(&invalid)).is_err(),
            "accepted {label}"
        );
    }
    fs::remove_file(invalid).unwrap();
}

#[test]
fn reset_tail_cannot_hide_current_checksum_failure_or_uncommitted_transaction() {
    let (fixture, original, end) = reset_wal_fixture();
    let invalid = fixture.directory.0.join("invalid-current-wal");
    let mut corrupt = original.clone();
    corrupt[32 + 24] ^= 1;
    fs::write(&invalid, corrupt).unwrap();
    assert!(
        validate_container(&fixture.database, Some(&invalid))
            .unwrap_err()
            .to_string()
            .contains("WAL frame checksum differs")
    );
    let mut uncommitted = original;
    uncommitted[end - (4096 + 24) + 4..end - (4096 + 24) + 8].fill(0);
    // Update only synthetic current-generation rolling checksums so the next
    // refusal proves the commit-boundary guard, not incidental corruption.
    let little_endian = be32(&uncommitted, 0) & 1 == 0;
    let mut sums = [be32(&uncommitted, 24), be32(&uncommitted, 28)];
    for frame in uncommitted[32..end].chunks_exact_mut(4096 + 24) {
        for part in [&frame[..8], &frame[24..]] {
            for chunk in part.chunks_exact(8) {
                let word = |bytes: &[u8]| {
                    let array = bytes.try_into().unwrap();
                    if little_endian {
                        u32::from_le_bytes(array)
                    } else {
                        u32::from_be_bytes(array)
                    }
                };
                sums[0] = sums[0]
                    .wrapping_add(word(&chunk[..4]))
                    .wrapping_add(sums[1]);
                sums[1] = sums[1]
                    .wrapping_add(word(&chunk[4..]))
                    .wrapping_add(sums[0]);
            }
        }
        frame[16..20].copy_from_slice(&sums[0].to_be_bytes());
        frame[20..24].copy_from_slice(&sums[1].to_be_bytes());
    }
    for data in [&uncommitted[..], &uncommitted[..end]] {
        fs::write(&invalid, data).unwrap();
        let message = validate_container(&fixture.database, Some(&invalid))
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("committed current prefix")
                || message.contains("uncommitted transaction")
        );
    }
    fs::remove_file(invalid).unwrap();
}

#[test]
fn authorizer_denies_mutation_attach_and_checkpoint() {
    let fixture = Fixture::new(false);
    let connection = open_readonly(&fixture.database).unwrap();
    for sql in [
        "DELETE FROM item",
        "ATTACH ':memory:' AS other",
        "PRAGMA wal_checkpoint",
        "PRAGMA writable_schema=ON",
    ] {
        assert!(connection.execute_batch(sql).is_err());
    }
}

#[test]
fn artwork_union_retains_old_cached_ids_and_uncached_registrations() {
    let fixture = Fixture::new(true);
    fixture
        .connection
        .execute_batch(
            "INSERT INTO artwork VALUES('100',300,6,0,'aa/00000000000000000000000000000000000000');
         INSERT INTO artwork VALUES('677',300,6,0,'ab/00000000000000000000000000000000000000');
         INSERT INTO artwork VALUES('677',300,1,1,'ac/00000000000000000000000000000000000000');
         INSERT INTO artwork_token VALUES(1,'676',300,1,0,0);
         INSERT INTO artwork_token VALUES(31,'677',300,6,4,0);
         INSERT INTO artwork_token VALUES(999,'4294967295',300,7,9,2);",
        )
        .unwrap();
    let catalog = fixture.read(true).unwrap();
    let artwork = catalog.artwork();
    assert_eq!(
        artwork.occupied_cache_ids(),
        &BTreeSet::from([100, 676, 677, u32::MAX])
    );
    assert_eq!(artwork.cached_ids(), &BTreeSet::from([100, 677]));
    assert_eq!(
        artwork.track_cache_ids(),
        &BTreeMap::from([(u64::MAX, 676)])
    );
    assert_eq!(
        artwork.album_cache_ids(),
        &BTreeMap::from([(-31i64 as u64, 677)])
    );
    assert_eq!(artwork.source300_rows().cached_rows, 3);
    assert_eq!(artwork.source300_rows().registration_rows, 3);
    assert_eq!(artwork.unmapped_source300_token_rows(), 1);
    assert!(artwork.other_source_rows().is_empty());
    assert!(!artwork.cached_ids().contains(&676));
    assert_eq!(
        artwork.tracks_with_artwork_registration(),
        &BTreeSet::from([u64::MAX])
    );
}

#[test]
fn foreign_artwork_tokens_are_counted_without_namespace_interpretation() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch(
            "INSERT INTO artwork VALUES('cloud:opaque-token',500,6,0,'uninterpreted');
         INSERT INTO artwork VALUES(X'00ff',500,NULL,NULL,NULL);
         INSERT INTO artwork_token VALUES(NULL,'not-decimal',500,NULL,NULL,NULL);
         INSERT INTO artwork_token VALUES(1,NULL,700,1,0,0);",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    let artwork = catalog.artwork();
    assert!(artwork.occupied_cache_ids().is_empty());
    assert!(artwork.track_cache_ids().is_empty());
    assert!(artwork.album_cache_ids().is_empty());
    assert_eq!(
        artwork.tracks_with_artwork_registration(),
        &BTreeSet::from([u64::MAX])
    );
    assert_eq!(artwork.other_source_rows()[&500].cached_rows, 2);
    assert_eq!(artwork.other_source_rows()[&500].registration_rows, 1);
    assert_eq!(artwork.other_source_rows()[&700].registration_rows, 1);
}

#[test]
fn artwork_presence_covers_foreign_and_unknown_track_shapes_without_token_interpretation() {
    for registration in [
        "INSERT INTO artwork_token VALUES(1,'opaque:foreign',500,1,0,0)",
        "INSERT INTO artwork_token VALUES(1,X'00ff',700,NULL,0,NULL)",
        "INSERT INTO artwork_token VALUES(1,NULL,500,999,0,888)",
        "INSERT INTO artwork_token VALUES(1,'678',300,99,0,77)",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute(registration, []).unwrap();
        let catalog = fixture.read(false).unwrap();
        let art = catalog.artwork();
        assert_eq!(
            art.tracks_with_artwork_registration(),
            &BTreeSet::from([u64::MAX])
        );
        assert!(art.track_cache_ids().is_empty());
        if registration.contains(",300,") {
            assert_eq!(art.occupied_cache_ids(), &BTreeSet::from([678]));
            assert_eq!(art.unmapped_source300_token_rows(), 1);
        } else {
            assert!(art.occupied_cache_ids().is_empty());
        }
    }
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch("INSERT INTO artwork_token VALUES(1,'a',500,1,0,0),(1,'b',500,2,0,1),(31,'c',500,6,4,0)").unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(
        catalog.artwork().tracks_with_artwork_registration(),
        &BTreeSet::from([u64::MAX])
    );
    assert!(catalog.artwork().occupied_cache_ids().is_empty());
}

#[test]
fn artwork_presence_refuses_unresolved_declared_track_registrations() {
    for entity in ["NULL", "0", "999", "'invalid'"] {
        let fixture = Fixture::new(false);
        fixture
            .connection
            .execute(
                &format!("INSERT INTO artwork_token VALUES({entity},'opaque',500,NULL,0,NULL)"),
                [],
            )
            .unwrap();
        assert!(fixture.read(false).is_err(), "entity {entity}");
    }
}

#[test]
fn source300_tokens_must_be_bounded_canonical_decimal_u32_text() {
    for table in ["artwork", "artwork_token"] {
        for token in [
            "",
            "0",
            "99",
            "0677",
            " 677",
            "677 ",
            "+677",
            "-677",
            "677x",
            "６７７",
            "4294967296",
            "10000000000",
        ] {
            let fixture = Fixture::new(false);
            fixture
                .connection
                .execute(
                    &format!(
                        "INSERT INTO {table}(artwork_token,artwork_source_type) VALUES(?1,300)"
                    ),
                    [token],
                )
                .unwrap();
            let error = fixture.read(false).err().expect("invalid token must fail");
            assert!(
                error.to_string().contains("source300 artwork token"),
                "{table}: {token:?}: {error}"
            );
        }
        for token_sql in ["NULL", "X'363737'", "CAST(X'ff' AS TEXT)"] {
            let fixture = Fixture::new(false);
            fixture
                .connection
                .execute_batch(&format!(
                    "INSERT INTO {table}(artwork_token,artwork_source_type) VALUES({token_sql},300)"
                ))
                .unwrap();
            let error = fixture
                .read(false)
                .err()
                .expect("nontext or invalid text must fail");
            assert!(
                error.to_string().contains("source300 artwork token"),
                "{table}: {token_sql}: {error}"
            );
        }
    }
}

/// A removal session that deletes an emptied album keeps its cover token (seen
/// in a 100-target removal): the ID stays occupied and belongs to no album.
#[test]
fn an_album_cover_token_without_its_album_stays_occupied() {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch(
            "INSERT INTO artwork_token VALUES(31,'677',300,6,4,0),(999,'678',300,6,4,0);
             INSERT INTO artwork VALUES('678',300,6,0,'ab/00000000000000000000000000000000000000');",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    let artwork = catalog.artwork();
    assert_eq!(artwork.occupied_cache_ids(), &BTreeSet::from([677, 678]));
    assert_eq!(
        artwork.album_cache_ids(),
        &BTreeMap::from([(-31i64 as u64, 677)])
    );
    assert_eq!(
        artwork.orphan_album_cache_ids(),
        &BTreeMap::from([(999, 678)])
    );
    assert_eq!(artwork.unmapped_source300_token_rows(), 0);
}

#[test]
fn duplicate_known_artwork_relationships_and_missing_joins_fail() {
    for sql in [
        "INSERT INTO artwork_token VALUES(1,'677',300,1,0,0),(1,'677',300,1,0,0)",
        "INSERT INTO artwork_token VALUES(1,'677',300,1,0,0),(1,'678',300,1,0,0)",
        "INSERT INTO artwork_token VALUES(31,'677',300,6,4,0),(31,'677',300,6,4,0)",
        "INSERT INTO artwork_token VALUES(31,'677',300,6,4,0),(31,'678',300,6,4,0)",
        "INSERT INTO artwork_token VALUES(999,'677',300,1,0,0)",
        "INSERT INTO artwork_token VALUES(0,'677',300,1,0,0)",
        "INSERT INTO artwork_token VALUES(NULL,'677',300,1,0,0)",
        "INSERT INTO artwork_token VALUES(1,'677',300,'unknown',0,0)",
        "INSERT INTO artwork_token VALUES(1,'677','unknown',1,0,0)",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
}

#[test]
fn artwork_schema_and_row_bounds_are_checked_before_token_decoding() {
    for sql in [
        "DROP TABLE artwork",
        "ALTER TABLE artwork_token RENAME COLUMN entity_type TO missing",
        "ALTER TABLE artwork RENAME TO old_artwork; CREATE TABLE artwork(artwork_token TEXT PRIMARY KEY,artwork_source_type INTEGER,artwork_type INTEGER,artwork_variant_type INTEGER)",
    ] {
        let fixture = Fixture::new(false);
        fixture.connection.execute_batch(sql).unwrap();
        assert!(fixture.read(false).is_err(), "{sql}");
    }
    let fixture = Fixture::new(false);
    fixture.connection.execute_batch(&format!(
        "WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<{}) INSERT INTO artwork SELECT 'invalid',300,6,0,NULL FROM n", MAX_ROWS + 1
    )).unwrap();
    let error = fixture
        .read(false)
        .err()
        .expect("row bound must fail first");
    assert!(error.to_string().contains("catalog table row limit"));
}

#[test]
fn cached_artwork_paths_and_duplicate_objects_fail_closed() {
    for path in [
        "",
        "../escape",
        "ab/0000000000000000000000000000000000000/",
        "AB/00000000000000000000000000000000000000",
        "ab/0000000000000000000000000000000000000g",
    ] {
        let fixture = Fixture::new(false);
        fixture
            .connection
            .execute("INSERT INTO artwork VALUES('677',300,6,0,?1)", [path])
            .unwrap();
        assert!(fixture.read(false).is_err());
    }
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute_batch(
            "INSERT INTO artwork VALUES('677',300,6,0,'ab/00000000000000000000000000000000000000')",
        )
        .unwrap();
    let catalog = fixture.read(false).unwrap();
    assert_eq!(
        catalog.artwork().cached_objects()[&(677, 0)].relative_path,
        "ab/00000000000000000000000000000000000000"
    );
    fixture
        .connection
        .execute_batch(
            "INSERT INTO artwork VALUES('677',300,6,0,'ab/00000000000000000000000000000000000000')",
        )
        .unwrap();
    assert!(fixture.read(false).is_err());
}

#[test]
fn artwork_recent_pair_fingerprint_excludes_only_the_two_recent_counters() {
    let catalog = |play: u32, skip: u32, date: &str, opaque: &str| {
        modified_collection_catalog(
            12,
            &format!(
                "ALTER TABLE item_stats ADD COLUMN date_skipped REAL; ALTER TABLE item_stats ADD COLUMN future_stat; UPDATE item_stats SET play_count_recent={play},skip_count_recent={skip},date_skipped={date},future_stat={opaque};"
            ),
        )
    };
    let before = catalog(3, 2, "123.25", "1");
    let reset = catalog(0, 0, "123.25", "1");
    assert_ne!(
        before.stats_except_recent_play(),
        reset.stats_except_recent_play()
    );
    assert_eq!(
        before.stats_except_recent_play_and_skip(),
        reset.stats_except_recent_play_and_skip()
    );
    let play_only = catalog(0, 2, "123.25", "1");
    assert_eq!(
        before.stats_except_recent_play(),
        play_only.stats_except_recent_play()
    );
    for changed in [
        catalog(0, 0, "123.5", "1"),
        catalog(0, 0, "123.25", "1.0"),
        catalog(0, 0, "123.25", "'1'"),
        catalog(0, 0, "123.25", "x'31'"),
        catalog(0, 0, "NULL", "1"),
    ] {
        assert_ne!(
            before.stats_except_recent_play_and_skip(),
            changed.stats_except_recent_play_and_skip()
        );
    }
}

mod memory_temp;
