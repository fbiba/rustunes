use super::*;

pub(super) fn i(n: i64) -> Cell {
    Cell::Integer(n)
}
pub(super) fn s(v: &str) -> Cell {
    Cell::Text(v.into())
}
pub(super) fn table(columns: &[&str], rows: Vec<Row>) -> Table {
    Table {
        columns: columns.iter().map(|v| (*v).into()).collect(),
        definitions: Vec::new(),
        rows: rows.into_iter().map(|r| (r, 1)).collect(),
    }
}

#[test]
fn maintenance_clock_advance_is_accepted_and_other_properties_stay_exact() {
    let (mut before, mut after) = fixture();
    let properties = |date: &str, anchor: &str| {
        table(
            &["key", "value"],
            vec![
                vec![s("MLMaintenanceTaskOperationDate"), s(date)],
                vec![s("MLSyncClientSyncAnchor"), s(anchor)],
            ],
        )
    };
    before.insert(
        "_MLDatabaseProperties".into(),
        properties("750196509.88088095", "141"),
    );
    after.insert(
        "_MLDatabaseProperties".into(),
        properties("750282909.209113", "141"),
    );
    let proof = verify_tables(&before, &after, &Budget::new()).unwrap();
    assert_eq!(
        proof["maintenance_task_operation_date"]["after"],
        "750282909.209113"
    );
    after.insert(
        "_MLDatabaseProperties".into(),
        properties("750282909.209113", "142"),
    );
    assert!(verify_tables(&before, &after, &Budget::new()).is_err());
}
pub(super) fn fixture() -> (BTreeMap<String, Table>, BTreeMap<String, Table>) {
    let mut a = BTreeMap::new();
    a.insert(
        "item".into(),
        table(
            &["item_pid", "title"],
            vec![vec![i(1), s("one")], vec![i(2), s("two")]],
        ),
    );
    a.insert(
        "other".into(),
        table(&["id", "value"], vec![vec![i(1), s("preserved")]]),
    );
    a.insert(
        "item_stats".into(),
        table(
            &[
                "item_pid",
                "play_count_user",
                "play_count_recent",
                "has_been_played",
                "date_played",
                "user_rating",
                "skip_count_user",
                "skip_count_recent",
                "date_skipped",
            ],
            vec![
                vec![i(1), i(0), i(0), i(0), i(0), i(0), i(0), i(0), i(0)],
                vec![i(2), i(4), i(1), i(1), i(1), i(0), i(0), i(0), i(0)],
            ],
        ),
    );
    a.insert(
        "container".into(),
        table(
            &[
                "container_pid",
                "distinguished_kind",
                "name",
                "is_hidden",
                "is_editable",
                "is_owner",
                "contained_media_type",
                "is_src_remote",
                "sync_id",
                "date_modified",
            ],
            vec![
                vec![
                    i(9),
                    i(10000),
                    s("Playback History"),
                    i(1),
                    i(0),
                    i(1),
                    i(8),
                    i(0),
                    i(0),
                    i(1),
                ],
                vec![
                    i(10),
                    i(0),
                    s("User playlist"),
                    i(0),
                    i(1),
                    i(1),
                    i(8),
                    i(0),
                    i(55),
                    i(1),
                ],
            ],
        ),
    );
    a.insert(
        "container_item".into(),
        table(
            &[
                "container_item_pid",
                "container_pid",
                "item_pid",
                "position",
                "uuid",
                "position_uuid",
                "occurrence_id",
            ],
            vec![
                vec![i(101), i(9), i(1), i(0), s(""), s(""), s("")],
                vec![i(102), i(9), i(2), i(1), s(""), s(""), s("")],
                vec![i(201), i(10), i(1), i(0), s(""), s(""), s("")],
            ],
        ),
    );
    a.insert(
        "entity_revision".into(),
        table(
            &[
                "revision",
                "entity_pid",
                "deleted",
                "class",
                "revision_type",
            ],
            vec![
                vec![i(10), i(1), i(0), i(0), i(1)],
                vec![i(11), i(9), i(0), i(1), i(0)],
            ],
        ),
    );
    let mut b = a.clone();
    change(&mut b, "item_stats", "item_pid", 1, "play_count_user", i(2));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        1,
        "play_count_recent",
        i(2),
    );
    change(&mut b, "item_stats", "item_pid", 1, "has_been_played", i(1));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        1,
        "date_played",
        Cell::Real(2.5f64.to_bits()),
    );
    change(
        &mut b,
        "container",
        "container_pid",
        9,
        "date_modified",
        Cell::Real(3.5f64.to_bits()),
    );
    let t = b.get_mut("container_item").unwrap();
    let key = t.column("container_item_pid").unwrap();
    t.rows.retain(|r, _| r[key] != i(101));
    t.rows
        .insert(vec![i(103), i(9), i(1), i(1), s(""), s(""), s("")], 1);
    change(
        &mut b,
        "container_item",
        "container_item_pid",
        102,
        "position",
        i(0),
    );
    change(
        &mut b,
        "entity_revision",
        "entity_pid",
        1,
        "revision",
        i(12),
    );
    change(
        &mut b,
        "entity_revision",
        "entity_pid",
        9,
        "revision",
        i(13),
    );
    (a, b)
}
pub(super) fn change(
    tables: &mut BTreeMap<String, Table>,
    table: &str,
    key: &str,
    id: i64,
    column: &str,
    value: Cell,
) {
    let t = tables.get_mut(table).unwrap();
    let mut row = t.one(key, id).unwrap().clone();
    t.rows.remove(&row);
    row[t.column(column).unwrap()] = value;
    t.rows.insert(row, 1);
}
#[test]
fn accepts_only_counter_increments_recognized_queue_and_matching_revisions() {
    let (a, b) = fixture();
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["completed_events"][0]["play_count_delta"], 2);
    assert_eq!(proof["playback_history"]["dropped_oldest"], 1);
    assert_eq!(proof["playback_history"]["retained_ordered_suffix"], 1);
    assert_eq!(proof["playback_history"]["appended"][0]["item_pid"], 1);
    assert_eq!(proof["entity_revisions"].as_array().unwrap().len(), 2);
    verify_tables(&a, &a, &Budget::new()).unwrap();
}
#[test]
fn refuses_counter_decrease_mismatched_increment_rating_skip_and_date_changes() {
    for (field, value) in [
        ("play_count_user", i(-1)),
        ("play_count_recent", i(1)),
        ("user_rating", i(5)),
        ("skip_count_user", i(1)),
        ("date_played", i(0)),
        ("date_played", Cell::Real(f64::NAN.to_bits())),
        ("has_been_played", i(0)),
    ] {
        let (a, mut b) = fixture();
        change(&mut b, "item_stats", "item_pid", 1, field, value);
        assert!(verify_tables(&a, &b, &Budget::new()).is_err(), "{field}");
    }
}
#[test]
fn refuses_stats_membership_and_unrelated_table_or_container_edits() {
    let (a, mut b) = fixture();
    b.get_mut("item_stats").unwrap().rows.pop_first();
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    let (a, mut b) = fixture();
    change(&mut b, "other", "id", 1, "value", s("changed"));
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    let (a, mut b) = fixture();
    change(
        &mut b,
        "container",
        "container_pid",
        10,
        "date_modified",
        i(4),
    );
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    let (a, mut b) = fixture();
    change(
        &mut b,
        "container",
        "container_pid",
        9,
        "name",
        s("Anything else"),
    );
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
}
#[test]
fn refuses_arbitrary_reorder_foreign_items_reused_ids_and_history_shrink() {
    let (a, mut b) = fixture();
    change(
        &mut b,
        "container_item",
        "container_item_pid",
        102,
        "position",
        i(1),
    );
    change(
        &mut b,
        "container_item",
        "container_item_pid",
        103,
        "position",
        i(0),
    );
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    for (field, value) in [
        ("item_pid", i(99)),
        ("container_item_pid", i(101)),
        ("container_item_pid", i(0)),
        ("container_item_pid", i(-1)),
        ("uuid", s("unknown")),
    ] {
        let (a, mut b) = fixture();
        change(
            &mut b,
            "container_item",
            "container_item_pid",
            103,
            field,
            value,
        );
        assert!(verify_tables(&a, &b, &Budget::new()).is_err(), "{field}");
    }
    let (a, mut b) = fixture();
    let t = b.get_mut("container_item").unwrap();
    let row = t.one("container_item_pid", 103).unwrap().clone();
    t.rows.remove(&row);
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
}

#[test]
fn retained_history_may_name_tracks_removed_before() {
    // A removal keeps a removed track's history rows: entry 102 names item
    // 77, gone from the library before either capture.
    let (mut a, mut b) = fixture();
    for tables in [&mut a, &mut b] {
        change(
            tables,
            "container_item",
            "container_item_pid",
            102,
            "item_pid",
            i(77),
        );
    }
    verify_tables(&a, &b, &Budget::new()).unwrap();
    // A new entry still has to name a track in the library.
    change(
        &mut b,
        "container_item",
        "container_item_pid",
        103,
        "item_pid",
        i(77),
    );
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
}

#[test]
fn independent_history_events_need_no_completed_play_counter_increment() {
    let (a, mut b) = fixture();
    b.insert("item_stats".into(), a["item_stats"].clone());
    change(
        &mut b,
        "entity_revision",
        "entity_pid",
        1,
        "revision",
        i(10),
    );
    for (id, item, position) in [(104, 2, 2), (105, 1, 3)] {
        b.get_mut("container_item").unwrap().rows.insert(
            vec![i(id), i(9), i(item), i(position), s(""), s(""), s("")],
            1,
        );
    }
    let proof = verify_tables(&a, &b, &Budget::new())
        .expect("history starts/selections are independent of completed-play counts");
    assert!(proof["completed_events"].as_array().unwrap().is_empty());
    assert_eq!(
        proof["playback_history"]["appended"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        proof["playback_history"]["completed_play_attribution_claimed"],
        false
    );
    let mut edited = b.clone();
    change(&mut edited, "item", "item_pid", 2, "title", s("changed"));
    assert!(verify_tables(&a, &edited, &Budget::new()).is_err());
    let mut revised = b.clone();
    revised
        .get_mut("entity_revision")
        .unwrap()
        .rows
        .insert(vec![i(14), i(2), i(0), i(0), i(1)], 1);
    assert!(verify_tables(&a, &revised, &Budget::new()).is_err());
    let mut stats = b;
    change(&mut stats, "item_stats", "item_pid", 2, "date_played", i(9));
    assert!(verify_tables(&a, &stats, &Budget::new()).is_err());
}

#[test]
fn completed_play_counts_need_no_history_event() {
    let (a, mut b) = fixture();
    for table in ["container", "container_item"] {
        b.insert(table.into(), a[table].clone());
    }
    change(
        &mut b,
        "entity_revision",
        "entity_pid",
        9,
        "revision",
        i(11),
    );
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["completed_events"][0]["play_count_delta"], 2);
    assert!(proof["playback_history"].is_null());
}

#[test]
fn history_event_counts_do_not_claim_per_item_completed_play_attribution() {
    let (a, mut b) = fixture();
    change(&mut b, "item_stats", "item_pid", 1, "play_count_user", i(1));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        1,
        "play_count_recent",
        i(1),
    );
    change(&mut b, "item_stats", "item_pid", 2, "play_count_user", i(6));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        2,
        "play_count_recent",
        i(3),
    );
    change(&mut b, "item_stats", "item_pid", 2, "date_played", i(4));
    b.get_mut("entity_revision")
        .unwrap()
        .rows
        .insert(vec![i(14), i(2), i(0), i(0), i(1)], 1);
    let history = b.get_mut("container_item").unwrap();
    let container = history.column("container_pid").unwrap();
    history.rows.retain(|row, _| row[container] != i(9));
    for position in 0..3 {
        history.rows.insert(
            vec![
                i(103 + position),
                i(9),
                i(1),
                i(position),
                s(""),
                s(""),
                s(""),
            ],
            1,
        );
    }
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["playback_history"]["appended_per_item"]["1"], 3);
    assert_eq!(proof["completed_events"][0]["play_count_delta"], 1);
    assert_eq!(proof["completed_events"][1]["play_count_delta"], 2);
    assert_eq!(
        proof["playback_history"]["completed_play_attribution_claimed"],
        false
    );
}
pub(super) fn bookkeeping_fixture() -> (BTreeMap<String, Table>, BTreeMap<String, Table>) {
    let (mut a, mut b) = fixture();
    for tables in [&mut a, &mut b] {
        let revisions = &mut tables.get_mut("entity_revision").unwrap().rows;
        revisions.insert(vec![i(4), i(1), i(0), i(0), i(0)], 1);
        revisions.insert(vec![i(5), i(2), i(0), i(0), i(0)], 1);
        revisions.insert(vec![i(3), i(90), i(2), i(0), i(0)], 1);
    }
    let revisions = &mut b.get_mut("entity_revision").unwrap().rows;
    revisions.remove(&vec![i(4), i(1), i(0), i(0), i(0)]);
    revisions.insert(vec![i(14), i(1), i(0), i(0), i(0)], 1);
    revisions.remove(&vec![i(5), i(2), i(0), i(0), i(0)]);
    revisions.insert(vec![i(15), i(2), i(0), i(0), i(0)], 1);
    revisions.remove(&vec![i(12), i(1), i(0), i(0), i(1)]);
    (a, b)
}
#[test]
fn newer_live_full_record_revisions_cover_same_item_stats_without_changing_content() {
    let (a, b) = bookkeeping_fixture();
    let proof = verify_tables(&a, &b, &Budget::new())
        .expect("same-item live full-record invalidation covers the superseded stats revision");
    assert_eq!(proof["completed_events"][0]["play_count_delta"], 2);
    assert!(
        proof["entity_revisions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["meaning"] == "stats_revision_superseded_by_full_record")
    );
}
#[test]
fn revision_bookkeeping_refuses_uncovered_stats_tombstone_foreign_or_flag_changes() {
    for invalid in 0..6 {
        let (a, mut b) = bookkeeping_fixture();
        let revisions = &mut b.get_mut("entity_revision").unwrap().rows;
        match invalid {
            0 => {
                // Removal cannot borrow a different item's newer full revision.
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
                revisions.insert(vec![i(4), i(1), i(0), i(0), i(0)], 1);
            }
            1 => {
                revisions.remove(&vec![i(3), i(90), i(2), i(0), i(0)]);
            }
            2 => {
                revisions.insert(vec![i(16), i(99), i(0), i(0), i(0)], 1);
            }
            3 => {
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
                revisions.insert(vec![i(14), i(1), i(2), i(0), i(0)], 1);
            }
            4 => {
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
                revisions.insert(vec![i(9), i(1), i(0), i(0), i(0)], 1);
            }
            5 => {
                // No full revision for the changed item and no stats coverage.
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
            }
            _ => unreachable!(),
        }
        assert!(
            verify_tables(&a, &b, &Budget::new()).is_err(),
            "case {invalid}"
        );
    }
}

#[test]
fn refuses_missing_nonadvancing_deleted_or_unrelated_revisions() {
    for (field, value) in [
        ("revision", i(11)),
        ("deleted", i(1)),
        ("revision_type", i(2)),
    ] {
        let (a, mut b) = fixture();
        change(&mut b, "entity_revision", "entity_pid", 1, field, value);
        assert!(verify_tables(&a, &b, &Budget::new()).is_err(), "{field}");
    }
    let (a, mut b) = fixture();
    let t = b.get_mut("entity_revision").unwrap();
    let row = t.one("entity_pid", 1).unwrap().clone();
    t.rows.remove(&row);
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    let (a, mut b) = fixture();
    b.get_mut("entity_revision")
        .unwrap()
        .rows
        .insert(vec![i(14), i(2), i(0), i(0), i(1)], 1);
    assert!(verify_tables(&a, &b, &Budget::new()).is_err());
}
#[test]
fn refuses_expired_parent_deadline() {
    let (a, b) = fixture();
    assert!(
        verify_tables(
            &a,
            &b,
            &Budget::new_until(Instant::now() - std::time::Duration::from_secs(1))
        )
        .is_err()
    );
}

pub(super) fn play_and_skip_fixture() -> (BTreeMap<String, Table>, BTreeMap<String, Table>) {
    let (a, mut b) = fixture();
    change(&mut b, "item_stats", "item_pid", 1, "skip_count_user", i(1));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        1,
        "skip_count_recent",
        i(1),
    );
    change(&mut b, "item_stats", "item_pid", 1, "date_skipped", i(3));
    (a, b)
}

#[test]
fn completed_plays_and_skips_are_accepted_for_every_recovery() {
    let (a, b) = play_and_skip_fixture();
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["completed_events"][0]["play_count_delta"], 2);
    assert_eq!(proof["completed_events"][0]["skip_count_delta"], 1);
}

#[test]
fn ordinary_native_counter_overflow_negative_and_mismatched_channels_refuse() {
    for invalid in 0..4 {
        let (a, mut b) = play_and_skip_fixture();
        let (field, value) = match invalid {
            0 => ("play_count_user", i(i64::from(u32::MAX) + 1)),
            1 => ("skip_count_recent", i(-1)),
            2 => ("skip_count_recent", i(2)),
            3 => ("play_count_recent", i(1)),
            _ => unreachable!(),
        };
        change(&mut b, "item_stats", "item_pid", 1, field, value);
        assert!(verify_tables(&a, &b, &Budget::new()).is_err());
    }
}

#[test]
fn ordinary_play_skip_counts_are_independent_of_history_attribution() {
    let (a, mut b) = play_and_skip_fixture();
    b.insert("container".into(), a["container"].clone());
    b.insert("container_item".into(), a["container_item"].clone());
    change(
        &mut b,
        "entity_revision",
        "entity_pid",
        9,
        "revision",
        i(11),
    );
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["completed_events"][0]["skip_count_delta"], 1);
    assert!(proof["playback_history"].is_null());
}

#[test]
fn state_scope_rejects_new_revision_device_prefs_cdb_and_raw_artwork_changes() {
    let current = |anchor| {
        let mut prefs = vec![0u8; 1232];
        prefs[..4].copy_from_slice(b"frpd");
        prefs[4] = 1;
        prefs[6] = 25;
        prefs[8] = 1;
        prefs[12] = 1;
        prefs[20] = 2;
        let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
        Current {
            device: "synthetic-device".into(),
            identity: crate::engine::state::journal::Identity::new("synthetic-device", &library)
                .unwrap(),
            library,
            catalog: music_protocol::test_support::catalogue::collection_catalog(anchor),
            raw_preferences: prefs.clone(),
            plist_preferences: prefs,
            cdb: vec![1, 2, 3],
            artwork_db: Some(vec![4, 5, 6]),
            receipt_sha256: [9; 32],
        }
    };
    let old = current(69);
    same_scope(&old, &current(69)).unwrap();
    assert!(same_scope(&old, &current(70)).is_err());
    let mut bad = current(69);
    bad.device.push('X');
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.raw_preferences[100] ^= 1;
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.plist_preferences[100] ^= 1;
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.cdb.push(1);
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.artwork_db = None;
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.artwork_db.as_mut().unwrap().push(1);
    assert!(same_scope(&old, &bad).is_err());
}
