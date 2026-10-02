use super::*;

fn mixed() -> SourceMetadataPatch {
    SourceMetadataPatch {
        title: SourceFieldRequest::Set("Composed title".into()),
        sort_name: SourceFieldRequest::Set("Composed sort".into()),
        composer: SourceFieldRequest::Set("Literal; Composer".into()),
        year: SourceFieldRequest::Set(2025),
        track: SourceFieldRequest::Set(7),
        ..Default::default()
    }
}

fn serialize(catalog: &Catalog, patch: &SourceMetadataPatch) -> Result<Vec<u8>> {
    let binding = bind_source_metadata(catalog, u64::MAX)?;
    prepare_source_metadata(catalog, &binding, patch, SystemTime::UNIX_EPOCH)
}

#[test]
fn source_metadata_composed_mixed_fields_one_track_operation() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let bytes = serialize(&catalog, &mixed()).unwrap();
    let expected = Value::from_reader_xml(std::io::Cursor::new(
        r#"<?xml version="1.0" encoding="UTF-8"?>
        <plist version="1.0"><dict>
          <key>revision</key><integer>13</integer>
          <key>timestamp</key><date>1970-01-01T00:00:00Z</date>
          <key>operations</key><array>
            <dict><key>operation</key><string>update_db_info</string>
              <key>pid</key><integer>-10</integer>
              <key>db_info</key><dict><key>primary_container_pid</key><integer>-11</integer></dict>
            </dict>
            <dict><key>operation</key><string>update_track</string>
              <key>pid</key><integer>-1</integer>
              <key>item</key><dict>
                <key>composer</key><string>Literal; Composer</string>
                <key>track_number</key><integer>7</integer>
                <key>year</key><integer>2025</integer>
                <key>title</key><string>Composed title</string>
                <key>sort_name</key><string>Composed sort</string>
                <key>album_pid</key><integer>-31</integer>
              </dict>
            </dict>
          </array>
        </dict></plist>"#,
    ))
    .unwrap();
    assert_eq!(
        Value::from_reader(std::io::Cursor::new(&bytes)).unwrap(),
        expected
    );
    assert_eq!(bytes, serialize(&catalog, &mixed()).unwrap());
    // Serialization observes the catalogue; it cannot advance its anchor.
    assert_eq!(catalog.persisted_media_anchor(), 12);
    assert_eq!(catalog.tracks()[0].metadata.title, "Synthetic Title");
    assert_eq!(catalog.tracks()[0].metadata.track_number, 6);
}

#[test]
fn source_metadata_single_family_wire_matches_legacy_serializers_byte_exact() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let title = TitlePatch {
        title: "Composed title".into(),
        sort_name: "Composed sort".into(),
    };
    let title_request = SourceMetadataPatch {
        title: SourceFieldRequest::Set(title.title.clone()),
        sort_name: SourceFieldRequest::Set(title.sort_name.clone()),
        ..Default::default()
    };
    assert_eq!(
        serialize(&catalog, &title_request).unwrap(),
        prepare_title(
            &catalog,
            &bind_title(&catalog, u64::MAX).unwrap(),
            &title,
            SystemTime::UNIX_EPOCH
        )
        .unwrap()
    );
    let scalar = ScalarPatch {
        composer: Some("Literal; Composer".into()),
        genre: Some("Explicit genre".into()),
        disc_number: Some(2),
        disc_count: Some(3),
        track_number: Some(7),
        track_count: Some(12),
        year: Some(2025),
    };
    let scalar_request = SourceMetadataPatch {
        composer: SourceFieldRequest::Set("Literal; Composer".into()),
        genre: SourceFieldRequest::Set("Explicit genre".into()),
        disc: SourceFieldRequest::Set(2),
        disc_total: SourceFieldRequest::Set(3),
        track: SourceFieldRequest::Set(7),
        track_total: SourceFieldRequest::Set(12),
        year: SourceFieldRequest::Set(2025),
        ..Default::default()
    };
    assert_eq!(
        serialize(&catalog, &scalar_request).unwrap(),
        prepare(&catalog, u64::MAX, &scalar, SystemTime::UNIX_EPOCH).unwrap()
    );
    let number = SourceMetadataPatch {
        track: SourceFieldRequest::Set(7),
        ..Default::default()
    };
    assert_eq!(
        serialize(&catalog, &number).unwrap(),
        prepare_track_number(
            &catalog,
            &bind_track_number(&catalog, u64::MAX).unwrap(),
            7,
            SystemTime::UNIX_EPOCH
        )
        .unwrap()
    );
}

#[test]
fn source_metadata_unsupported_mixed_requests_refuse_the_whole_patch() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    for (variant, field, reason) in [
        (
            0,
            "artist",
            "group_identity_or_source_grouping_update_unqualified",
        ),
        (
            1,
            "album",
            "group_identity_or_source_grouping_update_unqualified",
        ),
        (
            2,
            "album_artist",
            "group_identity_or_source_grouping_update_unqualified",
        ),
        (
            3,
            "grouping_artist",
            "group_identity_or_source_grouping_update_unqualified",
        ),
        (4, "copyright", "copyright_update_encoding_unqualified"),
        (
            5,
            "content_rating",
            "content_rating_mapping_and_update_unqualified",
        ),
        (6, "compilation", "compilation_grouping_update_unqualified"),
        (
            7,
            "sample_rate",
            "audio_attribute_update_requires_replacement",
        ),
        (
            8,
            "bit_depth",
            "audio_attribute_update_requires_replacement",
        ),
        (9, "channels", "audio_attribute_update_requires_replacement"),
        (
            10,
            "duration_ticks",
            "audio_attribute_update_requires_replacement",
        ),
        (
            11,
            "total_time_ms",
            "audio_attribute_update_requires_replacement",
        ),
    ] {
        let mut request = mixed();
        match variant {
            0 => request.artist = SourceFieldRequest::Set("New artist".into()),
            1 => request.album = SourceFieldRequest::Set("New album".into()),
            2 => request.album_artist = SourceFieldRequest::Set("New album artist".into()),
            3 => request.grouping_artist = SourceFieldRequest::Set("New grouping".into()),
            4 => request.copyright = SourceFieldRequest::Set("New copyright".into()),
            5 => request.content_rating = SourceFieldRequest::Set(1),
            6 => request.compilation = SourceFieldRequest::Set(true),
            7 => request.sample_rate = SourceFieldRequest::Set(48000),
            8 => request.bit_depth = SourceFieldRequest::Set(24),
            9 => request.channels = SourceFieldRequest::Set(1),
            10 => request.duration_ticks = SourceFieldRequest::Set(123),
            _ => request.total_time_ms = SourceFieldRequest::Set(123),
        }
        assert_eq!(
            serialize(&catalog, &request).unwrap_err().to_string(),
            format!("source metadata field {field}: {reason}")
        );
    }
    assert_eq!(catalog.persisted_media_anchor(), 12);
}

#[test]
fn source_metadata_every_explicit_clear_and_numeric_zero_range_refuses() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let names = [
        "title",
        "sort_name",
        "artist",
        "album",
        "album_artist",
        "grouping_artist",
        "genre",
        "composer",
        "copyright",
        "year",
        "content_rating",
        "compilation",
        "track",
        "track_total",
        "disc",
        "disc_total",
        "sample_rate",
        "bit_depth",
        "channels",
        "duration_ticks",
        "total_time_ms",
    ];
    for (variant, field) in names.into_iter().enumerate() {
        let mut request = SourceMetadataPatch {
            year: SourceFieldRequest::Set(2025),
            ..Default::default()
        };
        match variant {
            0 => request.title = SourceFieldRequest::Clear,
            1 => request.sort_name = SourceFieldRequest::Clear,
            2 => request.artist = SourceFieldRequest::Clear,
            3 => request.album = SourceFieldRequest::Clear,
            4 => request.album_artist = SourceFieldRequest::Clear,
            5 => request.grouping_artist = SourceFieldRequest::Clear,
            6 => request.genre = SourceFieldRequest::Clear,
            7 => request.composer = SourceFieldRequest::Clear,
            8 => request.copyright = SourceFieldRequest::Clear,
            9 => request.year = SourceFieldRequest::Clear,
            10 => request.content_rating = SourceFieldRequest::Clear,
            11 => request.compilation = SourceFieldRequest::Clear,
            12 => request.track = SourceFieldRequest::Clear,
            13 => request.track_total = SourceFieldRequest::Clear,
            14 => request.disc = SourceFieldRequest::Clear,
            15 => request.disc_total = SourceFieldRequest::Clear,
            16 => request.sample_rate = SourceFieldRequest::Clear,
            17 => request.bit_depth = SourceFieldRequest::Clear,
            18 => request.channels = SourceFieldRequest::Clear,
            19 => request.duration_ticks = SourceFieldRequest::Clear,
            _ => request.total_time_ms = SourceFieldRequest::Clear,
        }
        assert_eq!(
            serialize(&catalog, &request).unwrap_err().to_string(),
            format!("source metadata field {field}: metadata_clearing_not_supported")
        );
    }
    for field in ["year", "track", "track_total", "disc", "disc_total"] {
        for (value, reason) in [
            (0, "metadata_clearing_not_supported"),
            (65536, "scalar_value_exceeds_u16_encoding"),
            (u32::MAX, "scalar_value_exceeds_u16_encoding"),
        ] {
            let mut request = SourceMetadataPatch::default();
            let change = SourceFieldRequest::Set(value);
            match field {
                "year" => request.year = change,
                "track" => request.track = change,
                "track_total" => request.track_total = change,
                "disc" => request.disc = change,
                _ => request.disc_total = change,
            }
            assert_eq!(
                serialize(&catalog, &request).unwrap_err().to_string(),
                format!("source metadata field {field}: {reason}")
            );
        }
    }
    assert!(
        serialize(
            &catalog,
            &SourceMetadataPatch {
                year: SourceFieldRequest::Set(65535),
                ..Default::default()
            }
        )
        .is_ok()
    );
}

#[test]
fn source_metadata_noop_invalid_text_and_numbering_refuse() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    assert_eq!(
        serialize(&catalog, &SourceMetadataPatch::default())
            .unwrap_err()
            .to_string(),
        "empty source metadata patch"
    );
    for request in [
        SourceMetadataPatch {
            title: SourceFieldRequest::Set("Synthetic Title".into()),
            ..Default::default()
        },
        SourceMetadataPatch {
            sort_name: SourceFieldRequest::Set("Synthetic Title".into()),
            ..Default::default()
        },
        SourceMetadataPatch {
            title: SourceFieldRequest::Set("Synthetic Title".into()),
            sort_name: SourceFieldRequest::Set("Synthetic Title".into()),
            year: SourceFieldRequest::Set(2025),
            ..Default::default()
        },
        SourceMetadataPatch {
            composer: SourceFieldRequest::Set("New composer".into()),
            year: SourceFieldRequest::Set(2026),
            ..Default::default()
        },
        SourceMetadataPatch {
            track: SourceFieldRequest::Set(11),
            ..Default::default()
        },
        SourceMetadataPatch {
            disc: SourceFieldRequest::Set(2),
            disc_total: SourceFieldRequest::Set(1),
            ..Default::default()
        },
    ] {
        assert!(serialize(&catalog, &request).is_err());
    }
    for invalid in [String::new(), "bad\0text".into(), "x".repeat(4097)] {
        for field in ["title", "sort_name", "composer", "genre"] {
            let mut request = mixed();
            match field {
                "title" => request.title = SourceFieldRequest::Set(invalid.clone()),
                "sort_name" => request.sort_name = SourceFieldRequest::Set(invalid.clone()),
                "composer" => request.composer = SourceFieldRequest::Set(invalid.clone()),
                _ => request.genre = SourceFieldRequest::Set(invalid.clone()),
            }
            assert!(serialize(&catalog, &request).is_err(), "{field}");
        }
    }
    let mut request = mixed();
    request.title = SourceFieldRequest::Set("   ".into());
    assert!(serialize(&catalog, &request).is_err());
}

#[test]
fn source_metadata_partial_title_requests_preserve_exact_observed_pair() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let binding = bind_title(&catalog, u64::MAX).unwrap();
    for (request, expected) in [
        (
            SourceMetadataPatch {
                title: SourceFieldRequest::Set("New title".into()),
                ..Default::default()
            },
            TitlePatch {
                title: "New title".into(),
                sort_name: binding.before().sort_name.clone(),
            },
        ),
        (
            SourceMetadataPatch {
                sort_name: SourceFieldRequest::Set("New sort".into()),
                ..Default::default()
            },
            TitlePatch {
                title: binding.before().title.clone(),
                sort_name: "New sort".into(),
            },
        ),
    ] {
        assert_eq!(
            serialize(&catalog, &request).unwrap(),
            prepare_title(&catalog, &binding, &expected, SystemTime::UNIX_EPOCH).unwrap()
        );
    }
}

#[test]
fn source_metadata_binding_rejects_forged_or_stale_complete_before_state() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let binding = bind_source_metadata(&catalog, u64::MAX).unwrap();
    for variant in 0..17 {
        let mut forged = binding.clone();
        match variant {
            0 => forged.title.target = 0,
            1 => forged.title.target = 123,
            2 => forged.title.album_pid = 0,
            3 => forged.title.database_pid = 0,
            4 => forged.title.primary_container_pid = 0,
            5 => forged.title.anchor += 1,
            6 => forged.title.database_sha256[0] ^= 1,
            7 => forged.title.wal_sha256 = Some([1; 32]),
            8 => forged.title.database_bytes += 1,
            9 => forged.title.wal_bytes = Some(123),
            10 => forged.title.before.title.push('!'),
            11 => forged.title.before.sort_name.push('!'),
            12 => forged.before.metadata.composer = None,
            13 => forged.before.metadata.year += 1,
            14 => forged.before.artist_sync_id = 0,
            15 => forged.before.playback.user_rating += 1,
            _ => forged.before.afc_path = None,
        }
        assert!(
            prepare_source_metadata(&catalog, &forged, &mixed(), SystemTime::UNIX_EPOCH).is_err()
        );
    }
    for sql in [
        "UPDATE item_stats SET user_rating=20",
        "UPDATE item_extra SET copyright='changed elsewhere'",
        "UPDATE item_extra SET content_rating_level=42",
    ] {
        let current = crate::test_support::catalogue::modified_collection_catalog(12, sql);
        assert!(
            prepare_source_metadata(&current, &binding, &mixed(), SystemTime::UNIX_EPOCH).is_err()
        );
    }
    for target in [0, 123] {
        assert!(bind_source_metadata(&catalog, target).is_err());
    }
    for sql in [
        "UPDATE item_extra SET sort_title=NULL",
        "UPDATE item_extra SET sort_title=''",
        "UPDATE item_extra SET title=''",
        "UPDATE item_state SET is_placeholder=1",
        "UPDATE item_state SET has_non_purgeable_asset=0",
    ] {
        let current = crate::test_support::catalogue::modified_collection_catalog(12, sql);
        assert!(bind_source_metadata(&current, u64::MAX).is_err());
    }
    assert!(
        bind_source_metadata(
            &crate::test_support::catalogue::collection_catalog(u32::MAX),
            u64::MAX
        )
        .is_err()
    );
}

#[test]
fn source_metadata_request_json_requires_complete_unique_typed_fields() {
    let request = mixed();
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 21);
    assert_eq!(
        serde_json::from_value::<SourceMetadataPatch>(value.clone()).unwrap(),
        request
    );
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("year");
    assert!(serde_json::from_value::<SourceMetadataPatch>(missing).is_err());
    let mut extra = value.clone();
    extra["execution_authorized"] = serde_json::json!(true);
    assert!(serde_json::from_value::<SourceMetadataPatch>(extra).is_err());
    for bad in [
        serde_json::json!({"action":"set"}),
        serde_json::json!({"action":"set","value":null}),
        serde_json::json!({"action":"set","value":"2025"}),
        serde_json::json!({"action":"set","value":2025,"override":true}),
        serde_json::json!({"action":"clear","value":2025}),
        serde_json::json!({"action":"preserve","value":2025}),
    ] {
        let mut changed = value.clone();
        changed["year"] = bad;
        assert!(serde_json::from_value::<SourceMetadataPatch>(changed).is_err());
    }
    let raw = serde_json::to_string(&request).unwrap();
    let duplicate_field = raw.replace("\"year\":", "\"year\":{\"action\":\"preserve\"},\"year\":");
    assert_ne!(duplicate_field, raw);
    assert!(serde_json::from_str::<SourceMetadataPatch>(&duplicate_field).is_err());
    let needle = "\"year\":{\"action\":\"set\",\"value\":2025}";
    assert!(raw.contains(needle));
    let duplicate_value = raw.replace(
        needle,
        "\"year\":{\"action\":\"set\",\"value\":2025,\"value\":2024}",
    );
    assert!(serde_json::from_str::<SourceMetadataPatch>(&duplicate_value).is_err());
}

#[test]
fn source_metadata_composition_preserves_signed_pid_bits_at_boundaries() {
    let cases = [1_u64, i64::MAX as u64, 1_u64 << 63, u64::MAX];
    for i in 0..cases.len() {
        let [target, album, database, primary] =
            std::array::from_fn(|n| cases[(i + n) % cases.len()] as i64);
        let catalog = crate::test_support::catalogue::modified_collection_catalog(
            12,
            &format!(
                "UPDATE item_store SET sync_id={target}; UPDATE album SET sync_id={album} WHERE album_pid=31; UPDATE db_info SET db_pid={database},primary_container_pid={primary}; UPDATE container SET sync_id={primary};"
            ),
        );
        let binding = bind_source_metadata(&catalog, target as u64).unwrap();
        let value = Value::from_reader(std::io::Cursor::new(
            prepare_source_metadata(&catalog, &binding, &mixed(), SystemTime::UNIX_EPOCH).unwrap(),
        ))
        .unwrap();
        let ops = value.as_dictionary().unwrap()["operations"]
            .as_array()
            .unwrap();
        assert_eq!(ops.len(), 2);
        let db = ops[0].as_dictionary().unwrap();
        let own = ops[1].as_dictionary().unwrap();
        assert_eq!(db["pid"].as_signed_integer(), Some(database));
        assert_eq!(
            db["db_info"].as_dictionary().unwrap()["primary_container_pid"].as_signed_integer(),
            Some(primary)
        );
        assert_eq!(own["pid"].as_signed_integer(), Some(target));
        assert_eq!(
            own["item"].as_dictionary().unwrap()["album_pid"].as_signed_integer(),
            Some(album)
        );
    }
}

#[test]
fn source_metadata_shared_case_lookup_refuses_and_observed_null_preserves() {
    let shared = crate::test_support::catalogue::modified_collection_catalog(
        12,
        concat!(
            "INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;",
            "INSERT INTO item_extra SELECT 2,title,'OTHER.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;",
            "INSERT INTO item_store VALUES(2,123); INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;",
            "INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;"
        ),
    );
    for request in [
        SourceMetadataPatch {
            composer: SourceFieldRequest::Set("SYNTHETIC COMPOSER".into()),
            year: SourceFieldRequest::Set(2025),
            ..Default::default()
        },
        SourceMetadataPatch {
            genre: SourceFieldRequest::Set("SYNTHETIC GENRE".into()),
            year: SourceFieldRequest::Set(2025),
            ..Default::default()
        },
    ] {
        assert!(serialize(&shared, &request).is_err());
    }
    let absent = crate::test_support::catalogue::modified_collection_catalog(
        12,
        "UPDATE item SET composer_pid=0,genre_id=0",
    );
    let binding = bind_source_metadata(&absent, u64::MAX).unwrap();
    assert!(binding.before().metadata.composer.is_none());
    assert!(binding.before().metadata.genre.is_none());
    let value = Value::from_reader(std::io::Cursor::new(
        prepare_source_metadata(
            &absent,
            &binding,
            &SourceMetadataPatch {
                year: SourceFieldRequest::Set(2025),
                ..Default::default()
            },
            SystemTime::UNIX_EPOCH,
        )
        .unwrap(),
    ))
    .unwrap();
    let ops = value.as_dictionary().unwrap()["operations"]
        .as_array()
        .unwrap();
    let item = ops[1].as_dictionary().unwrap()["item"]
        .as_dictionary()
        .unwrap();
    assert_eq!(item.len(), 1);
    assert_eq!(item["year"].as_unsigned_integer(), Some(2025));
    assert!(!item.contains_key("composer") && !item.contains_key("genre"));
}

#[test]
fn source_metadata_projection_requests_preserve_set_clear_every_field() {
    // Both inputs cross the actual ProjectedFields decoder. Expected requests
    // below are independent literals, not another implementation of comparison.
    let before: crate::media::metadata::ProjectedFields = serde_json::from_value(serde_json::json!({
        "title":"Old title", "sort_name":"Old sort", "artist":"Old artist", "album":"Old album",
        "album_artist":null, "grouping_artist":"Old grouping", "genre":"Old genre", "composer":"Old composer",
        "copyright":null, "year":2026, "content_rating":null, "compilation":false,
        "track":6, "track_total":10, "disc":1, "disc_total":null,
        "sample_rate":44100, "bit_depth":16, "channels":2, "duration_ticks":1000, "total_time_ms":20
    })).unwrap();
    assert_eq!(
        SourceMetadataPatch::from_projections(&before, &before),
        SourceMetadataPatch::default()
    );
    let desired_input = serde_json::json!({
        "title":"New title", "sort_name":"New sort", "artist":"New artist", "album":"New album",
        "album_artist":"New album artist", "grouping_artist":"New grouping", "genre":"New genre", "composer":"New composer",
        "copyright":"New copyright", "year":2042, "content_rating":2, "compilation":true,
        "track":7, "track_total":19, "disc":3, "disc_total":5,
        "sample_rate":48000, "bit_depth":24, "channels":1, "duration_ticks":222222, "total_time_ms":44444
    });
    let desired: crate::media::metadata::ProjectedFields =
        serde_json::from_value(desired_input.clone()).unwrap();
    let patch = SourceMetadataPatch::from_projections(&before, &desired);
    let expected = serde_json::json!({
        "title":{"action":"set","value":"New title"},
        "sort_name":{"action":"set","value":"New sort"},
        "artist":{"action":"set","value":"New artist"},
        "album":{"action":"set","value":"New album"},
        "album_artist":{"action":"set","value":"New album artist"},
        "grouping_artist":{"action":"set","value":"New grouping"},
        "genre":{"action":"set","value":"New genre"},
        "composer":{"action":"set","value":"New composer"},
        "copyright":{"action":"set","value":"New copyright"},
        "year":{"action":"set","value":2042},
        "content_rating":{"action":"set","value":2},
        "compilation":{"action":"set","value":true},
        "track":{"action":"set","value":7},
        "track_total":{"action":"set","value":19},
        "disc":{"action":"set","value":3},
        "disc_total":{"action":"set","value":5},
        "sample_rate":{"action":"set","value":48000},
        "bit_depth":{"action":"set","value":24},
        "channels":{"action":"set","value":1},
        "duration_ticks":{"action":"set","value":222222},
        "total_time_ms":{"action":"set","value":44444}
    });
    assert_eq!(serde_json::to_value(&patch).unwrap(), expected);

    // Different same-type payloads must follow their own actual desired member,
    // rather than a neighbouring member or a stale earlier request.
    let mut swapped_input = desired_input;
    swapped_input["track"] = serde_json::json!(3);
    swapped_input["disc"] = serde_json::json!(7);
    let swapped: crate::media::metadata::ProjectedFields =
        serde_json::from_value(swapped_input).unwrap();
    let swapped_patch = SourceMetadataPatch::from_projections(&before, &swapped);
    assert_eq!(swapped_patch.track, SourceFieldRequest::Set(3));
    assert_eq!(swapped_patch.disc, SourceFieldRequest::Set(7));
    assert_ne!(serde_json::to_value(&swapped_patch).unwrap(), expected);

    let removed: crate::media::metadata::ProjectedFields = serde_json::from_value(serde_json::json!({
        "title":"New title", "sort_name":"New sort", "artist":"New artist", "album":"New album",
        "album_artist":null, "grouping_artist":"New grouping", "genre":null, "composer":null,
        "copyright":null, "year":null, "content_rating":null, "compilation":null,
        "track":null, "track_total":null, "disc":null, "disc_total":null,
        "sample_rate":48000, "bit_depth":24, "channels":1, "duration_ticks":222222, "total_time_ms":44444
    })).unwrap();
    assert_eq!(
        serde_json::to_value(SourceMetadataPatch::from_projections(&desired, &removed)).unwrap(),
        serde_json::json!({
            "title":{"action":"preserve"}, "sort_name":{"action":"preserve"},
            "artist":{"action":"preserve"}, "album":{"action":"preserve"},
            "album_artist":{"action":"clear"}, "grouping_artist":{"action":"preserve"},
            "genre":{"action":"clear"}, "composer":{"action":"clear"}, "copyright":{"action":"clear"},
            "year":{"action":"clear"}, "content_rating":{"action":"clear"}, "compilation":{"action":"clear"},
            "track":{"action":"clear"}, "track_total":{"action":"clear"}, "disc":{"action":"clear"}, "disc_total":{"action":"clear"},
            "sample_rate":{"action":"preserve"}, "bit_depth":{"action":"preserve"}, "channels":{"action":"preserve"},
            "duration_ticks":{"action":"preserve"}, "total_time_ms":{"action":"preserve"}
        })
    );
    assert_eq!(
        serde_json::to_value(SourceMetadataPatch::from_projections(&removed, &removed)).unwrap(),
        serde_json::json!({
            "title":{"action":"preserve"}, "sort_name":{"action":"preserve"},
            "artist":{"action":"preserve"}, "album":{"action":"preserve"},
            "album_artist":{"action":"preserve"}, "grouping_artist":{"action":"preserve"},
            "genre":{"action":"preserve"}, "composer":{"action":"preserve"}, "copyright":{"action":"preserve"},
            "year":{"action":"preserve"}, "content_rating":{"action":"preserve"}, "compilation":{"action":"preserve"},
            "track":{"action":"preserve"}, "track_total":{"action":"preserve"}, "disc":{"action":"preserve"}, "disc_total":{"action":"preserve"},
            "sample_rate":{"action":"preserve"}, "bit_depth":{"action":"preserve"}, "channels":{"action":"preserve"},
            "duration_ticks":{"action":"preserve"}, "total_time_ms":{"action":"preserve"}
        })
    );
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    assert_eq!(
        serialize(&catalog, &patch).unwrap_err().to_string(),
        "source metadata field artist: group_identity_or_source_grouping_update_unqualified"
    );
}
