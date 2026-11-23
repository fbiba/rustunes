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

/// The phone's own revision housekeeping moments after an import, with no
/// listening: both live items' full records renumbered above every revision,
/// and item 1's statistics revision dropped as superseded.
fn housekeeping_fixture() -> (BTreeMap<String, Table>, BTreeMap<String, Table>) {
    let (mut a, _) = fixture();
    let revisions = &mut a.get_mut("entity_revision").unwrap().rows;
    revisions.insert(vec![i(4), i(1), i(0), i(0), i(0)], 1);
    revisions.insert(vec![i(5), i(2), i(0), i(0), i(0)], 1);
    revisions.insert(vec![i(3), i(90), i(2), i(0), i(0)], 1);
    let mut b = a.clone();
    let revisions = &mut b.get_mut("entity_revision").unwrap().rows;
    revisions.remove(&vec![i(4), i(1), i(0), i(0), i(0)]);
    revisions.insert(vec![i(14), i(1), i(0), i(0), i(0)], 1);
    revisions.remove(&vec![i(5), i(2), i(0), i(0), i(0)]);
    revisions.insert(vec![i(15), i(2), i(0), i(0), i(0)], 1);
    revisions.remove(&vec![i(10), i(1), i(0), i(0), i(1)]);
    (a, b)
}
#[test]
fn revision_housekeeping_accepts_renumbered_live_items_and_superseded_stats() {
    let (a, b) = housekeeping_fixture();
    let accepted = revision_housekeeping(&a, &b, &Budget::new()).unwrap();
    assert!(accepted.stray_rows.is_empty());
    let meanings: Vec<_> = accepted
        .revisions
        .iter()
        .map(|c| c["meaning"].clone())
        .collect();
    assert_eq!(
        meanings,
        [
            "live_item_full_record_revision_advanced",
            "stats_revision_superseded_by_full_record",
            "live_item_full_record_revision_advanced",
        ]
    );
    assert!(
        revision_housekeeping(&a, &a, &Budget::new())
            .unwrap()
            .revisions
            .is_empty()
    );
}
#[test]
fn revision_housekeeping_refuses_listening_and_every_other_change() {
    for invalid in 0..9 {
        let (a, mut b) = housekeeping_fixture();
        match invalid {
            0 => change(&mut b, "item_stats", "item_pid", 2, "play_count_user", i(5)),
            1 => change(
                &mut b,
                "container_item",
                "container_item_pid",
                102,
                "position",
                i(2),
            ),
            2 => change(&mut b, "other", "id", 1, "value", s("changed")),
            _ => {}
        }
        let revisions = &mut b.get_mut("entity_revision").unwrap().rows;
        match invalid {
            0..=2 => {}
            3 => {
                // A fresh statistics revision is listening, not housekeeping.
                revisions.insert(vec![i(16), i(2), i(0), i(0), i(1)], 1);
            }
            4 => {
                revisions.remove(&vec![i(11), i(9), i(0), i(1), i(0)]);
                revisions.insert(vec![i(16), i(9), i(0), i(1), i(0)], 1);
            }
            5 => {
                // Renumbered below a revision the earlier capture holds.
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
                revisions.insert(vec![i(9), i(1), i(0), i(0), i(0)], 1);
            }
            6 => {
                // A statistics revision dropped without its item renumbered.
                revisions.remove(&vec![i(14), i(1), i(0), i(0), i(0)]);
                revisions.insert(vec![i(4), i(1), i(0), i(0), i(0)], 1);
            }
            7 => {
                revisions.remove(&vec![i(3), i(90), i(2), i(0), i(0)]);
            }
            8 => {
                // Item 2 left out of the renumbering.
                revisions.remove(&vec![i(15), i(2), i(0), i(0), i(0)]);
                revisions.insert(vec![i(5), i(2), i(0), i(0), i(0)], 1);
            }
            _ => unreachable!(),
        }
        assert!(
            revision_housekeeping(&a, &b, &Budget::new()).is_err(),
            "case {invalid}"
        );
    }
}

/// A statistics row of `pid` at its defaults, in the fixtures' columns.
fn stats_row(pid: i64) -> Row {
    vec![i(pid), i(0), i(0), i(0), i(0), i(0), i(0), i(0), i(0)]
}

/// `rows` of `table`, keyed by item, added to `tables`, columns `item_pid`
/// and `location`.
fn add_located(tables: &mut BTreeMap<String, Table>, name: &str, rows: &[(i64, &str)]) {
    let t = tables
        .entry(name.into())
        .or_insert_with(|| table(&["item_pid", "location"], Vec::new()));
    for (pid, location) in rows {
        t.rows.insert(vec![i(*pid), s(location)], 1);
    }
}

#[test]
fn revision_housekeeping_takes_the_sweep_of_rows_whose_track_is_gone() {
    // Item 7 has no track: the sweep ending the import's session deletes
    // its statistics row.
    let (mut a, mut b) = housekeeping_fixture();
    a.get_mut("item_stats")
        .unwrap()
        .rows
        .insert(stats_row(7), 1);
    for tables in [&mut a, &mut b] {
        add_located(
            tables,
            "item_extra",
            &[(1, "one"), (2, "two"), (7, "seven")],
        );
    }
    let accepted = revision_housekeeping(&a, &b, &Budget::new()).unwrap();
    assert_eq!(accepted.revisions.len(), 3);
    assert_eq!(
        accepted.stray_rows.table("item_stats").unwrap().vanished,
        BTreeSet::from([7])
    );
    assert!(accepted.stray_rows.table("item_extra").is_none());
    // Nothing else of such rows changes in a run: not item_extra's (the
    // sanitize's), and none appears or changes (a later write).
    for case in 0..3 {
        let (mut a, mut b) = (a.clone(), b.clone());
        match case {
            0 => {
                b.get_mut("item_extra")
                    .unwrap()
                    .rows
                    .remove(&vec![i(7), s("seven")]);
            }
            1 => {
                b.get_mut("item_stats")
                    .unwrap()
                    .rows
                    .insert(stats_row(8), 1);
            }
            _ => {
                a.get_mut("item_extra")
                    .unwrap()
                    .rows
                    .remove(&vec![i(7), s("seven")]);
                add_located(&mut a, "item_extra", &[(7, "changed")]);
            }
        }
        assert!(
            revision_housekeeping(&a, &b, &Budget::new()).is_err(),
            "case {case}"
        );
    }
}

#[test]
fn the_bridge_takes_what_the_phone_did_to_rows_whose_track_is_gone_between_runs() {
    // Items 7 and 8 have no track. Between the write's run and the later
    // one, the phone's sanitize deleted item 7's rows but its item_state
    // row, and a late write added item 8's.
    let (mut a, mut b) = fixture();
    a.get_mut("item_stats")
        .unwrap()
        .rows
        .insert(stats_row(7), 1);
    add_located(
        &mut a,
        "item_extra",
        &[(1, "one"), (2, "two"), (7, "seven")],
    );
    add_located(
        &mut b,
        "item_extra",
        &[(1, "one"), (2, "two"), (8, "eight")],
    );
    add_located(
        &mut a,
        "item_state",
        &[(1, "one"), (2, "two"), (7, "seven")],
    );
    add_located(
        &mut b,
        "item_state",
        &[(1, "one"), (2, "two"), (7, "seven"), (8, "eight")],
    );
    let proof = verify_tables(&a, &b, &Budget::new()).unwrap();
    assert_eq!(proof["completed_events"][0]["item_pid"], 1);
    let strays = &proof["stray_rows"];
    assert_eq!(strays["item_extra"], json!({"vanished":[7],"appeared":[8]}));
    assert_eq!(strays["item_stats"], json!({"vanished":[7]}));
    assert_eq!(strays["item_state"], json!({"appeared":[8]}));
    // Nothing deletes a stray item_state row, and a track's own rows stay
    // exact.
    for case in 0..3 {
        let mut b = b.clone();
        match case {
            0 => {
                b.get_mut("item_state")
                    .unwrap()
                    .rows
                    .remove(&vec![i(7), s("seven")]);
            }
            1 => {
                b.get_mut("item_extra")
                    .unwrap()
                    .rows
                    .remove(&vec![i(2), s("two")]);
            }
            _ => {
                let t = b.get_mut("item_stats").unwrap();
                let row = t.one("item_pid", 2).unwrap().clone();
                t.rows.remove(&row);
            }
        }
        assert!(
            verify_tables(&a, &b, &Budget::new()).is_err(),
            "case {case}"
        );
    }
    // Without them the bridge reports none.
    let (a, b) = fixture();
    assert!(verify_tables(&a, &b, &Budget::new()).unwrap()["stray_rows"].is_null());
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
            cdb: DeviceFile::Present(vec![1, 2, 3]),
            artwork_db: DeviceFile::Present(vec![4, 5, 6]),
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
    bad.cdb = DeviceFile::Present(vec![1, 2, 3, 1]);
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.artwork_db = DeviceFile::NotCaptured;
    assert!(same_scope(&old, &bad).is_err());
    let mut bad = current(69);
    bad.artwork_db = DeviceFile::Present(vec![4, 5, 6, 1]);
    assert!(same_scope(&old, &bad).is_err());
    // A phone `rustunes init` prepared has neither file, and keeps neither.
    for (cdb, artwork) in [
        (DeviceFile::Absent, DeviceFile::Present(vec![4, 5, 6])),
        (DeviceFile::Present(vec![1, 2, 3]), DeviceFile::Absent),
    ] {
        let mut bad = current(69);
        bad.cdb = cdb;
        bad.artwork_db = artwork;
        assert!(same_scope(&old, &bad).is_err());
    }
    let unsynced = |anchor| {
        let mut current = current(anchor);
        current.cdb = DeviceFile::Absent;
        current.artwork_db = DeviceFile::Absent;
        current
    };
    same_scope(&unsynced(69), &unsynced(69)).unwrap();
}

/// The housekeeping and playback checks compare two captures of one database
/// and build no item_state row, so they read a database iOS 27.2 built new,
/// whose layout differs from a migrated one's (docs/04), and iOS 26's
/// (docs/09, section 8.4) as well; any other layout is still refused.
#[test]
fn full_table_comparisons_read_a_database_built_new_or_on_ios_26() {
    use music_protocol::test_support::catalogue::NativeFixture;
    use sha2::{Digest, Sha256};
    let snapshot = |fixture: &NativeFixture, database: &std::path::Path, name: &str| {
        let bytes = std::fs::read(database).unwrap();
        let capture = Capture {
            database,
            database_sha256: Sha256::digest(bytes).into(),
            wal: None,
        };
        reader::materialize(capture, &fixture.dir.join(name), &mut Budget::new()).unwrap()
    };
    for fixture in [
        NativeFixture::built_new(),
        NativeFixture::index_only_update(),
        NativeFixture::ios_26(),
    ] {
        let before = fixture.before.as_path();
        let (a, b) = (
            snapshot(&fixture, before, "a"),
            snapshot(&fixture, before, "b"),
        );
        assert!(
            verify_revision_housekeeping(&a, &b, &Budget::new())
                .unwrap()
                .revisions
                .is_empty()
        );
        verify_views(&a, &b, &Budget::new()).unwrap();
        // The phone's renumbering after an import: every live item's full
        // record above every revision the earlier capture holds (1 to 4).
        // Leaving one item out is refused.
        for (sql, renumbered) in [
            ("UPDATE entity_revision SET revision=revision+10", Some(4)),
            (
                "UPDATE entity_revision SET revision=revision+10 WHERE entity_pid<>4",
                None,
            ),
        ] {
            let path = fixture.dir.join("renumbered.sqlite");
            std::fs::copy(before, &path).unwrap();
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
            let name = format!("renumbered-{}", renumbered.is_some());
            let later = snapshot(&fixture, &path, &name);
            let result = verify_revision_housekeeping(&a, &later, &Budget::new());
            match renumbered {
                Some(count) => {
                    let changes = result.unwrap().revisions;
                    assert_eq!(changes.len(), count, "{sql}");
                    assert!(
                        changes
                            .iter()
                            .all(|c| c["meaning"] == "live_item_full_record_revision_advanced")
                    );
                }
                None => assert!(result.is_err(), "{sql}"),
            }
            std::fs::remove_file(&path).unwrap();
        }
        rusqlite::Connection::open(before)
            .unwrap()
            .execute_batch("ALTER TABLE item_state ADD COLUMN unmodelled INTEGER")
            .unwrap();
        let changed = snapshot(&fixture, before, "c");
        let error = verify_revision_housekeeping(&changed, &changed, &Budget::new()).unwrap_err();
        assert!(error.to_string().contains("unsupported"), "{error}");
        assert!(verify_views(&changed, &changed, &Budget::new()).is_err());
        std::fs::remove_dir_all(&fixture.dir).unwrap();
    }
}

/// Two captures of one run while music played: a completed play is taken by
/// `verify_listening_between` and refused by `verify_revision_housekeeping`,
/// which allows the phone's renumbering alone; a content change is refused
/// by both.
#[test]
fn a_play_between_two_captures_of_a_run_is_taken_as_listening_only() {
    use music_protocol::test_support::catalogue::NativeFixture;
    use sha2::{Digest, Sha256};
    let fixture = NativeFixture::built_new();
    let snapshot = |database: &std::path::Path, name: &str| {
        let bytes = std::fs::read(database).unwrap();
        let capture = Capture {
            database,
            database_sha256: Sha256::digest(bytes).into(),
            wal: None,
        };
        reader::materialize(capture, &fixture.dir.join(name), &mut Budget::new()).unwrap()
    };
    let before = fixture.before.as_path();
    let a = snapshot(before, "a");
    let edited = |sql: &str, name: &str| {
        let path = fixture.dir.join(format!("{name}.sqlite"));
        std::fs::copy(before, &path).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        snapshot(&path, name)
    };
    let played = edited(
        "UPDATE item_stats SET play_count_user=play_count_user+1, \
         play_count_recent=play_count_recent+1, has_been_played=1, \
         date_played=date_played+100 WHERE item_pid=(SELECT MIN(item_pid) FROM item); \
         INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type) \
         VALUES((SELECT MAX(revision)+1 FROM entity_revision), \
         (SELECT MIN(item_pid) FROM item),0,0,1)",
        "played",
    );
    assert!(verify_revision_housekeeping(&a, &played, &Budget::new()).is_err());
    let taken = verify_listening_between(&a, &played, &Budget::new()).unwrap();
    let listening = taken.listening.unwrap();
    assert_eq!(listening["completed_events"][0]["play_count_delta"], 1);
    assert!(taken.revisions.is_empty() && taken.stray_rows.is_empty());
    assert!(
        verify_listening_between(&a, &a, &Budget::new())
            .unwrap()
            .listening
            .is_none()
    );
    let renamed = edited(
        "UPDATE item_extra SET title='Renamed' WHERE item_pid=(SELECT MIN(item_pid) FROM item)",
        "renamed",
    );
    assert!(verify_revision_housekeeping(&a, &renamed, &Budget::new()).is_err());
    let error = verify_listening_between(&a, &renamed, &Budget::new()).unwrap_err();
    assert!(error.to_string().contains("item_extra"), "{error}");
    std::fs::remove_dir_all(&fixture.dir).unwrap();
}
