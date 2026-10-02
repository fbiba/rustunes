use super::*;
use crate::catalogue::metadata_update::{
    SourceFieldRequest::*, bind_source_metadata, prepare_source_metadata,
};
use crate::catalogue::sqlite::Catalog;
use std::time::{Duration, SystemTime};

fn mixed() -> SourceMetadataPatch {
    SourceMetadataPatch {
        title: Set("Composed title".into()),
        sort_name: Set("Composed sort".into()),
        composer: Set("New composer".into()),
        genre: Set("New genre".into()),
        year: Set(2025),
        track: Set(7),
        track_total: Set(12),
        disc: Set(2),
        disc_total: Set(3),
        ..Default::default()
    }
}
fn bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out).unwrap();
    out
}
fn prepared(catalog: &Catalog, patch: &SourceMetadataPatch) -> Vec<u8> {
    let binding = bind_source_metadata(catalog, u64::MAX).unwrap();
    prepare_source_metadata(catalog, &binding, patch, SystemTime::UNIX_EPOCH).unwrap()
}
fn check(catalog: &Catalog, patch: &SourceMetadataPatch, operation: &[u8]) -> Result<()> {
    let binding = bind_source_metadata(catalog, u64::MAX).unwrap();
    validate_source_metadata_operation_data(
        binding.before(),
        &binding.before_title().sort_name,
        patch,
        catalog.database_pid(),
        catalog.primary_container_pid(),
        catalog.persisted_media_anchor(),
        operation,
    )
}
fn ops(value: &mut Value) -> &mut Vec<Value> {
    value
        .as_dictionary_mut()
        .unwrap()
        .get_mut("operations")
        .unwrap()
        .as_array_mut()
        .unwrap()
}
fn item(value: &mut Value) -> &mut plist::Dictionary {
    ops(value)[1]
        .as_dictionary_mut()
        .unwrap()
        .get_mut("item")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
}

#[test]
fn data_validator_accepts_genuine_serializer_families_and_timestamps() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let binding = bind_source_metadata(&catalog, u64::MAX).unwrap();
    let requests = [
        SourceMetadataPatch {
            title: Set("New title".into()),
            ..Default::default()
        },
        SourceMetadataPatch {
            sort_name: Set("New sort".into()),
            ..Default::default()
        },
        SourceMetadataPatch {
            year: Set(2025),
            ..Default::default()
        },
        SourceMetadataPatch {
            title: Preserve,
            sort_name: Preserve,
            ..mixed()
        },
        mixed(),
        SourceMetadataPatch {
            title: Set("é".repeat(2048)),
            sort_name: Set("é".repeat(2048)),
            composer: Set("é".repeat(2048)),
            genre: Set("é".repeat(2048)),
            ..mixed()
        },
    ];
    for request in requests {
        for timestamp in [
            SystemTime::UNIX_EPOCH - Duration::from_secs(123),
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789),
        ] {
            let operation =
                prepare_source_metadata(&catalog, &binding, &request, timestamp).unwrap();
            check(&catalog, &request, &operation).unwrap();
        }
    }
    let nullable = crate::test_support::catalogue::modified_collection_catalog(
        12,
        "UPDATE item SET composer_pid=0,genre_id=0;",
    );
    for request in [
        SourceMetadataPatch {
            year: Set(2025),
            ..Default::default()
        },
        mixed(),
    ] {
        check(&nullable, &request, &prepared(&nullable, &request)).unwrap();
    }
}

#[test]
fn data_validator_rejects_changed_or_incomplete_envelopes_and_timestamp_shapes() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let request = mixed();
    let original = Value::from_reader(Cursor::new(prepared(&catalog, &request))).unwrap();
    for variant in 0..23 {
        let mut changed = original.clone();
        match variant {
            0 => {
                changed.as_dictionary_mut().unwrap().remove("revision");
            }
            1 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("revision".into(), 12u32.into());
            }
            2 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("revision".into(), 14u32.into());
            }
            3 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("revision".into(), "13".into());
            }
            4 => {
                changed.as_dictionary_mut().unwrap().remove("timestamp");
            }
            5 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("timestamp".into(), "1970-01-01T00:00:00Z".into());
            }
            6 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("timestamp".into(), 0u32.into());
            }
            7 => {
                changed
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("extra".into(), true.into());
            }
            8 => {
                changed.as_dictionary_mut().unwrap().remove("operations");
            }
            9 => {
                ops(&mut changed).reverse();
            }
            10 => {
                ops(&mut changed).pop();
            }
            11 => {
                let duplicate = ops(&mut changed)[1].clone();
                ops(&mut changed).push(duplicate);
            }
            12 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("pid".into(), (-100i64).into());
            }
            13 => {
                ops(&mut changed)[1]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("pid".into(), 123i64.into());
            }
            14 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .get_mut("db_info")
                    .unwrap()
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("primary_container_pid".into(), (-100i64).into());
            }
            15 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("operation".into(), "delete_track".into());
            }
            16 => {
                ops(&mut changed)[1]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("operation".into(), "insert_track".into());
            }
            17 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("extra".into(), true.into());
            }
            18 => {
                ops(&mut changed)[1]
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("extra".into(), true.into());
            }
            19 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .get_mut("db_info")
                    .unwrap()
                    .as_dictionary_mut()
                    .unwrap()
                    .insert("extra".into(), true.into());
            }
            20 => {
                ops(&mut changed)[0]
                    .as_dictionary_mut()
                    .unwrap()
                    .remove("db_info");
            }
            21 => {
                ops(&mut changed)[1]
                    .as_dictionary_mut()
                    .unwrap()
                    .remove("item");
            }
            _ => {
                changed = Value::Array(vec![changed]);
            }
        }
        assert!(
            check(&catalog, &request, &bytes(&changed)).is_err(),
            "variant {variant}"
        );
    }
}

#[test]
fn data_validator_rejects_each_omitted_or_changed_item_field() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let request = mixed();
    let mut original = Value::from_reader(Cursor::new(prepared(&catalog, &request))).unwrap();
    let keys: Vec<_> = item(&mut original).keys().cloned().collect();
    assert_eq!(keys.len(), 10); // Nine requested fields plus required album_pid.
    for key in keys {
        let mut missing = original.clone();
        item(&mut missing).remove(&key);
        assert!(
            check(&catalog, &request, &bytes(&missing)).is_err(),
            "missing {key}"
        );
        let mut wrong = original.clone();
        item(&mut wrong).insert(key.clone(), false.into());
        assert!(
            check(&catalog, &request, &bytes(&wrong)).is_err(),
            "wrong type {key}"
        );
        let mut wrong = original.clone();
        let value = match item(&mut wrong).get(&key).unwrap() {
            Value::String(_) => Value::String("Different accepted text".into()),
            _ => 1000u32.into(),
        };
        item(&mut wrong).insert(key.clone(), value);
        assert!(
            check(&catalog, &request, &bytes(&wrong)).is_err(),
            "wrong value {key}"
        );
    }
    item(&mut original).insert("copyright".into(), "Unrequested".into());
    assert!(check(&catalog, &request, &bytes(&original)).is_err());
}

#[test]
fn data_validator_shares_unsupported_clear_range_and_noop_rejections() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let binding = bind_source_metadata(&catalog, u64::MAX).unwrap();
    let operation = prepared(&catalog, &mixed());
    let valid = serde_json::to_value(mixed()).unwrap();
    let mut requests = vec![SourceMetadataPatch::default()];
    // All 21 explicit Clear requests must reject even beside supported changes.
    for field in valid.as_object().unwrap().keys() {
        let mut value = valid.clone();
        value[field] = serde_json::json!({"action":"clear"});
        requests.push(serde_json::from_value(value).unwrap());
    }
    for (field, value) in [
        ("artist", serde_json::json!("New")),
        ("album", serde_json::json!("New")),
        ("album_artist", serde_json::json!("New")),
        ("grouping_artist", serde_json::json!("New")),
        ("copyright", serde_json::json!("New")),
        ("content_rating", serde_json::json!(1)),
        ("compilation", serde_json::json!(true)),
        ("sample_rate", serde_json::json!(48000)),
        ("bit_depth", serde_json::json!(24)),
        ("channels", serde_json::json!(1)),
        ("duration_ticks", serde_json::json!(123)),
        ("total_time_ms", serde_json::json!(123)),
    ] {
        let mut changed = valid.clone();
        changed[field] = serde_json::json!({"action":"set","value":value});
        requests.push(serde_json::from_value(changed).unwrap());
    }
    for field in ["year", "track", "track_total", "disc", "disc_total"] {
        for number in [0, 65536] {
            let mut changed = valid.clone();
            changed[field] = serde_json::json!({"action":"set","value":number});
            requests.push(serde_json::from_value(changed).unwrap());
        }
    }
    requests.extend([
        SourceMetadataPatch {
            title: Set("".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            composer: Set("".into()),
            ..mixed()
        },
        SourceMetadataPatch {
            title: Set("Synthetic Title".into()),
            sort_name: Preserve,
            ..mixed()
        },
        SourceMetadataPatch {
            year: Set(2026),
            ..mixed()
        },
        SourceMetadataPatch {
            track: Set(13),
            ..mixed()
        },
    ]);
    for request in requests {
        let serializer =
            prepare_source_metadata(&catalog, &binding, &request, SystemTime::UNIX_EPOCH)
                .unwrap_err();
        let validator = check(&catalog, &request, &operation).unwrap_err();
        assert_eq!(serializer.to_string(), validator.to_string(), "{request:?}");
    }
}

#[test]
fn data_validator_binds_envelope_ids_and_signed_pid_boundaries() {
    for (target, album, database, primary) in [
        (i64::MIN, i64::MAX, -10, -11),
        (i64::MAX, i64::MIN, i64::MIN, i64::MAX),
        (-1, -31, -10, -11),
    ] {
        let catalog = crate::test_support::catalogue::modified_collection_catalog(
            12,
            &format!(
                "UPDATE item_store SET sync_id={target}; UPDATE album SET sync_id={album} WHERE album_pid=31; UPDATE db_info SET db_pid={database},primary_container_pid={primary}; UPDATE container SET sync_id={primary};"
            ),
        );
        let binding = bind_source_metadata(&catalog, target as u64).unwrap();
        let request = mixed();
        let operation =
            prepare_source_metadata(&catalog, &binding, &request, SystemTime::UNIX_EPOCH).unwrap();
        let validate = |before: &Track, sort: &str, db: u64, primary: u64, anchor| {
            validate_source_metadata_operation_data(
                before, sort, &request, db, primary, anchor, &operation,
            )
        };
        validate(
            binding.before(),
            &binding.before_title().sort_name,
            database as u64,
            primary as u64,
            12,
        )
        .unwrap();
        for variant in 0..8 {
            let mut before = binding.before().clone();
            let mut db = database as u64;
            let mut group = primary as u64;
            let mut anchor = 12;
            match variant {
                0 => before.sync_id = 123,
                1 => before.album_sync_id = 456,
                2 => db = 123,
                3 => group = 456,
                4 => anchor = 13,
                5 => anchor = u32::MAX,
                6 => db = 0,
                _ => group = 0,
            }
            assert!(
                validate(
                    &before,
                    &binding.before_title().sort_name,
                    db,
                    group,
                    anchor
                )
                .is_err(),
                "variant {variant}"
            );
        }
    }
}

// Append a fourth root entry that repeats the first key/value. Value parsing
// collapses it to the same dictionary; canonical-byte validation must still fail.
fn duplicate_root_entry(input: &[u8]) -> Vec<u8> {
    let trailer = &input[input.len() - 32..];
    let width = usize::from(trailer[6]);
    let refs = usize::from(trailer[7]);
    let u64_at = |slice: &[u8]| u64::from_be_bytes(slice.try_into().unwrap());
    let objects = usize::try_from(u64_at(&trailer[8..16])).unwrap();
    let top = usize::try_from(u64_at(&trailer[16..24])).unwrap();
    let table = usize::try_from(u64_at(&trailer[24..32])).unwrap();
    let offset = |n: usize| {
        input[table + n * width..table + (n + 1) * width]
            .iter()
            .fold(0usize, |v, b| (v << 8) | usize::from(*b))
    };
    let root = offset(top);
    assert_eq!(input[root], 0xd3);
    let keys = &input[root + 1..root + 1 + 3 * refs];
    let values = &input[root + 1 + 3 * refs..root + 1 + 6 * refs];
    let mut out = input[..table].to_vec();
    out.push(0xd4);
    out.extend_from_slice(keys);
    out.extend_from_slice(&keys[..refs]);
    out.extend_from_slice(values);
    out.extend_from_slice(&values[..refs]);
    let new_table = out.len();
    // Use eight-byte offsets to avoid a test helper's width threshold dependency.
    for n in 0..objects {
        out.extend_from_slice(&(offset(n) as u64).to_be_bytes());
    }
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    out.extend_from_slice(&[8, refs as u8]);
    out.extend_from_slice(&((objects + 1) as u64).to_be_bytes());
    out.extend_from_slice(&(objects as u64).to_be_bytes());
    out.extend_from_slice(&(new_table as u64).to_be_bytes());
    out
}

#[test]
fn data_validator_rejects_noncanonical_duplicate_and_truncated_bytes() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let request = mixed();
    let operation = prepared(&catalog, &request);
    let value = Value::from_reader(Cursor::new(&operation)).unwrap();
    let duplicate = duplicate_root_entry(&operation);
    assert_eq!(Value::from_reader(Cursor::new(&duplicate)).unwrap(), value);
    assert!(check(&catalog, &request, &duplicate).is_err());
    let mut xml = Vec::new();
    value.to_writer_xml(&mut xml).unwrap();
    let mut reversed = plist::Dictionary::new();
    let entries: Vec<_> = value.as_dictionary().unwrap().iter().collect();
    for (key, value) in entries.into_iter().rev() {
        reversed.insert(key.clone(), value.clone());
    }
    let reordered = bytes(&Value::Dictionary(reversed));
    assert_ne!(reordered, operation);
    let mut padded = operation.clone();
    padded.extend_from_slice(b"unexpected trailing bytes");
    for invalid in [
        xml,
        reordered,
        padded,
        operation[..operation.len() - 1].to_vec(),
        vec![],
        vec![0; 65_537],
    ] {
        assert!(check(&catalog, &request, &invalid).is_err());
    }
}

#[test]
fn data_validation_does_not_admit_shared_lookup_effects_or_create_a_binding() {
    let ordinary = crate::test_support::catalogue::collection_catalog(12);
    let shared = crate::test_support::catalogue::modified_collection_catalog(
        12,
        concat!(
            "INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;",
            "INSERT INTO item_extra SELECT 2,title,'OTHER.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;",
            "INSERT INTO item_store VALUES(2,123); INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;",
            "INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;"
        ),
    );
    let request = SourceMetadataPatch {
        composer: Set("SYNTHETIC COMPOSER".into()),
        year: Set(2025),
        ..Default::default()
    };
    let operation = prepared(&ordinary, &request);
    // The descriptive target and operation match. This does not inspect siblings.
    check(&shared, &request, &operation).unwrap();
    let binding = bind_source_metadata(&shared, u64::MAX).unwrap();
    assert!(
        prepare_source_metadata(&shared, &binding, &request, SystemTime::UNIX_EPOCH)
            .unwrap_err()
            .to_string()
            .contains("shared composer case-only")
    );
}

#[test]
fn data_validator_bounds_repeated_binary_object_traversal() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    // Thirty shared binary-array levels would expand to over a billion leaves.
    // Each object references its predecessor twice; there are no cyclic refs.
    let mut operation = b"bplist00".to_vec();
    let mut offsets = Vec::new();
    offsets.push(operation.len() as u64);
    operation.push(0x09); // true
    for child in 0u8..30 {
        offsets.push(operation.len() as u64);
        operation.extend_from_slice(&[0xa2, child, child]);
    }
    let table = operation.len();
    for offset in &offsets {
        operation.extend_from_slice(&offset.to_be_bytes());
    }
    operation.extend_from_slice(&[0; 6]);
    operation.extend_from_slice(&[8, 1]);
    operation.extend_from_slice(&(offsets.len() as u64).to_be_bytes());
    operation.extend_from_slice(&30u64.to_be_bytes());
    operation.extend_from_slice(&(table as u64).to_be_bytes());
    let error = check(&catalog, &mixed(), &operation).unwrap_err();
    assert!(format!("{error:#}").contains("source metadata plist traversal budget"));
}

#[test]
fn data_validator_rejects_trailer_allocation_and_offset_bounds_before_parsing() {
    let catalog = crate::test_support::catalogue::collection_catalog(12);
    let request = mixed();
    let original = prepared(&catalog, &request);
    let start = original.len() - 32;
    for count in [0u64, 129, u64::MAX] {
        let mut wrong = original.clone();
        wrong[start + 8..start + 16].copy_from_slice(&count.to_be_bytes());
        assert!(
            format!("{:#}", check(&catalog, &request, &wrong).unwrap_err())
                .contains("source metadata binary layout")
        );
    }
    for position in [start + 6, start + 7] {
        for width in [0, 3, 9] {
            let mut wrong = original.clone();
            wrong[position] = width;
            assert!(
                format!("{:#}", check(&catalog, &request, &wrong).unwrap_err())
                    .contains("source metadata binary layout")
            );
        }
    }
    for table in [0u64, start as u64, u64::MAX] {
        let mut wrong = original.clone();
        wrong[start + 24..].copy_from_slice(&table.to_be_bytes());
        assert!(
            format!("{:#}", check(&catalog, &request, &wrong).unwrap_err())
                .contains("source metadata binary layout")
        );
    }
    let width = usize::from(original[start + 6]);
    let table = usize::try_from(u64::from_be_bytes(
        original[start + 24..].try_into().unwrap(),
    ))
    .unwrap();
    let mut wrong = original.clone();
    wrong[table..table + width].fill(0);
    assert!(
        format!("{:#}", check(&catalog, &request, &wrong).unwrap_err())
            .contains("source metadata binary layout")
    );
    let mut wrong = original.clone();
    wrong[start + 16..start + 24].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(
        format!("{:#}", check(&catalog, &request, &wrong).unwrap_err())
            .contains("source metadata binary layout")
    );
}
