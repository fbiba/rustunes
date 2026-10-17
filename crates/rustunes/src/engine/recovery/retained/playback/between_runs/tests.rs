use super::super::tests::{
    bookkeeping_fixture, change, fixture, i, play_and_skip_fixture, s, table,
};
use super::*;

type Tables = BTreeMap<String, Table>;

/// Completes the bridge fixtures to the native playback scope: both skip
/// channels and the maintenance clock property.
fn native(mut a: Tables, mut b: Tables) -> (Tables, Tables) {
    for tables in [&mut a, &mut b] {
        let t = tables.get_mut("item_stats").unwrap();
        if !t.columns.iter().any(|c| c == "skip_count_recent") {
            t.columns
                .extend(["skip_count_recent".into(), "date_skipped".into()]);
            t.rows = t
                .rows
                .keys()
                .map(|old| {
                    let mut row = old.clone();
                    row.extend([i(0), i(0)]);
                    (row, 1)
                })
                .collect();
        }
        tables.insert(
            "_MLDatabaseProperties".into(),
            table(
                &["key", "value"],
                vec![
                    vec![s("MLMaintenanceTaskOperationDate"), s("750196509.88088095")],
                    vec![s("MLSyncClientSyncAnchor"), s("141")],
                ],
            ),
        );
    }
    (a, b)
}

fn absorbed(a: &Tables, b: &Tables) -> Result<(Tables, Option<Value>)> {
    let mut expected = a.clone();
    let report = absorb(&mut expected, b, &Budget::new(), None);
    match report {
        Ok(report) => Ok((expected, report)),
        Err(error) => {
            assert_eq!(&expected, a, "a refusal must leave the model unchanged");
            Err(error)
        }
    }
}

fn set_property(tables: &mut Tables, key: &str, value: &str) {
    let t = tables.get_mut("_MLDatabaseProperties").unwrap();
    let row = t.rows.keys().find(|r| r[0] == s(key)).unwrap().clone();
    t.rows.remove(&row);
    t.rows.insert(vec![s(key), s(value)], 1);
}

/// Item 2 had one recent play before the write. The write's sync session
/// cleared it, and two later plays counted from zero.
fn cleared_then_played() -> (Tables, Tables) {
    let (a, _) = native(fixture().0, fixture().0);
    let mut b = a.clone();
    change(&mut b, "item_stats", "item_pid", 2, "play_count_user", i(6));
    change(
        &mut b,
        "item_stats",
        "item_pid",
        2,
        "play_count_recent",
        i(2),
    );
    change(
        &mut b,
        "item_stats",
        "item_pid",
        2,
        "date_played",
        Cell::Real(5.0f64.to_bits()),
    );
    b.get_mut("entity_revision")
        .unwrap()
        .rows
        .insert(vec![i(14), i(2), i(0), i(0), i(1)], 1);
    (a, b)
}

#[test]
fn no_playback_difference_is_no_effect() {
    let (a, _) = native(fixture().0, fixture().0);
    let (expected, report) = absorbed(&a, &a).unwrap();
    assert!(report.is_none());
    assert_eq!(expected, a);
}

#[test]
fn listening_is_copied_into_the_model_and_content_tables_are_left_to_the_caller() {
    let (a, mut b) = native(play_and_skip_fixture().0, play_and_skip_fixture().1);
    b.insert(
        "other".into(),
        table(&["id", "value"], vec![vec![i(1), s("changed content")]]),
    );
    let (expected, report) = absorbed(&a, &b).unwrap();
    let report = report.unwrap();
    for name in TABLES {
        assert_eq!(expected[name], b[name], "{name}");
    }
    assert_eq!(expected["other"], a["other"]);
    assert_eq!(expected["item"], a["item"]);
    assert_eq!(report["completed_events"][0]["item_pid"], 1);
    assert_eq!(report["completed_events"][0]["play_count_delta"], 2);
    assert_eq!(report["completed_events"][0]["skip_count_delta"], 1);
    assert_eq!(report["playback_history"]["appended"][0]["item_pid"], 1);
    assert_eq!(report["recent_counter_resets"], json!([]));
}

#[test]
fn housekeeping_renumbering_and_dropped_statistics_revisions_are_absorbed() {
    let (a, b) = native(bookkeeping_fixture().0, bookkeeping_fixture().1);
    let (_, report) = absorbed(&a, &b).unwrap();
    let meanings: Vec<_> = report.unwrap()["entity_revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["meaning"].as_str().unwrap().to_owned())
        .collect();
    assert!(meanings.contains(&"live_item_full_record_revision_advanced".to_owned()));
    assert!(meanings.contains(&"stats_revision_superseded_by_full_record".to_owned()));

    // Housekeeping alone: every live item renumbered above the old maximum and
    // the earlier listening's statistics revision dropped, with no new event.
    let (a, _) = native(bookkeeping_fixture().0, bookkeeping_fixture().0);
    let mut b = a.clone();
    let revisions = &mut b.get_mut("entity_revision").unwrap().rows;
    for (old, new) in [
        (
            vec![i(4), i(1), i(0), i(0), i(0)],
            vec![i(12), i(1), i(0), i(0), i(0)],
        ),
        (
            vec![i(5), i(2), i(0), i(0), i(0)],
            vec![i(13), i(2), i(0), i(0), i(0)],
        ),
    ] {
        revisions.remove(&old);
        revisions.insert(new, 1);
    }
    revisions.remove(&vec![i(10), i(1), i(0), i(0), i(1)]);
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(expected["entity_revision"], b["entity_revision"]);
    assert_eq!(report.unwrap()["completed_events"], json!([]));
}

#[test]
fn recent_counters_cleared_by_the_write_then_counted_again_are_absorbed() {
    let (a, b) = cleared_then_played();
    let (_, report) = absorbed(&a, &b).unwrap();
    let report = report.unwrap();
    assert_eq!(report["completed_events"][0]["item_pid"], 2);
    assert_eq!(report["completed_events"][0]["play_count_delta"], 2);
    assert_eq!(
        report["recent_counter_resets"],
        json!([{"item_pid":2,"cleared":["play_count_recent"],
            "before":{"play_count_recent":1,"skip_count_recent":0},
            "after":{"play_count_recent":2,"skip_count_recent":0}}])
    );
}

#[test]
fn a_cleared_recent_counter_alone_is_absorbed_without_an_event() {
    let (a, _) = native(fixture().0, fixture().0);
    let mut b = a.clone();
    change(
        &mut b,
        "item_stats",
        "item_pid",
        2,
        "play_count_recent",
        i(0),
    );
    let (_, report) = absorbed(&a, &b).unwrap();
    let report = report.unwrap();
    assert_eq!(report["completed_events"], json!([]));
    assert_eq!(report["recent_counter_resets"][0]["item_pid"], 2);
}

#[test]
fn maintenance_clock_advance_is_absorbed() {
    let (a, _) = native(fixture().0, fixture().0);
    let mut b = a.clone();
    set_property(&mut b, "MLMaintenanceTaskOperationDate", "750282909.209113");
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(
        expected["_MLDatabaseProperties"],
        b["_MLDatabaseProperties"]
    );
    assert_eq!(
        report.unwrap()["maintenance_task_operation_date"]["after"],
        "750282909.209113"
    );
}

/// The Playback History's media type count covers its entries whose track is
/// in the library. Entry 101, for a track removed earlier, drops out of the
/// history and the new entry 103 plays item 1, so the count moves from 1 to 2.
fn media_type_fixture() -> (Tables, Tables) {
    let (a, b) = fixture();
    let (mut a, mut b) = native(a, b);
    for (tables, count) in [(&mut a, 1), (&mut b, 2)] {
        tables.insert(
            "item".into(),
            table(
                &["item_pid", "media_type"],
                vec![vec![i(1), i(8)], vec![i(2), i(8)]],
            ),
        );
        tables.insert(
            MEDIA_TYPES.into(),
            table(
                &["container_pid", "media_type", "count"],
                vec![vec![i(9), i(8), i(count)], vec![i(10), i(8), i(1)]],
            ),
        );
    }
    change(
        &mut a,
        "container_item",
        "container_item_pid",
        101,
        "item_pid",
        i(77),
    );
    (a, b)
}

#[test]
fn history_media_type_count_follows_entries_for_tracks_in_the_library() {
    let (a, b) = media_type_fixture();
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(expected[MEDIA_TYPES], b[MEDIA_TYPES]);
    assert_eq!(
        report.unwrap()["history_media_type_counts"],
        json!([{"media_type":8,"before":1,"after":2}])
    );
    let row = |container: i64, kind: i64, count: i64| vec![i(container), i(kind), i(count)];
    for (case, old, new) in [
        ("moved by more than the entries", row(9, 8, 2), row(9, 8, 3)),
        (
            "another playlist's count moved",
            row(10, 8, 1),
            row(10, 8, 2),
        ),
        ("a media type row replaced", row(9, 8, 2), row(9, 1, 2)),
    ] {
        let (a, mut b) = media_type_fixture();
        let rows = &mut b.get_mut(MEDIA_TYPES).unwrap().rows;
        rows.remove(&old);
        rows.insert(new, 1);
        assert!(absorbed(&a, &b).is_err(), "{case}");
    }
}

#[test]
fn changes_listening_cannot_explain_are_refused() {
    let cases: Vec<(&str, Box<dyn Fn(&mut Tables)>)> = vec![
        (
            "recent counter is neither kept nor cleared before the plays",
            Box::new(|b| change(b, "item_stats", "item_pid", 2, "play_count_recent", i(5))),
        ),
        (
            "lifetime count decreased",
            Box::new(|b| change(b, "item_stats", "item_pid", 2, "play_count_user", i(3))),
        ),
        (
            "rating changed",
            Box::new(|b| change(b, "item_stats", "item_pid", 2, "user_rating", i(5))),
        ),
        (
            "play date moved without a play",
            Box::new(|b| {
                change(b, "item_stats", "item_pid", 2, "play_count_user", i(4));
                change(b, "item_stats", "item_pid", 2, "play_count_recent", i(1));
            }),
        ),
        (
            "plays without a later play date",
            Box::new(|b| {
                change(
                    b,
                    "item_stats",
                    "item_pid",
                    2,
                    "date_played",
                    Cell::Integer(1),
                )
            }),
        ),
        (
            "statistics row removed",
            Box::new(|b| {
                let t = b.get_mut("item_stats").unwrap();
                let row = t.one("item_pid", 2).unwrap().clone();
                t.rows.remove(&row);
            }),
        ),
        (
            "user playlist changed",
            Box::new(|b| change(b, "container", "container_pid", 10, "date_modified", i(9))),
        ),
        (
            "foreign revision added",
            Box::new(|b| {
                b.get_mut("entity_revision")
                    .unwrap()
                    .rows
                    .insert(vec![i(15), i(77), i(0), i(3), i(0)], 1);
            }),
        ),
        (
            "another database property changed",
            Box::new(|b| set_property(b, "MLSyncClientSyncAnchor", "142")),
        ),
        (
            "maintenance clock went backwards",
            Box::new(|b| set_property(b, "MLMaintenanceTaskOperationDate", "750196500")),
        ),
        (
            "playback table definition changed",
            Box::new(|b| {
                b.get_mut("container_item").unwrap().definitions = vec![vec![json!("x")]];
            }),
        ),
    ];
    for (case, edit) in cases {
        let (a, mut b) = cleared_then_played();
        edit(&mut b);
        assert!(absorbed(&a, &b).is_err(), "{case}");
    }
}

#[test]
fn refuses_expired_deadline() {
    let (a, b) = cleared_then_played();
    let mut expected = a.clone();
    let expired = Budget::new_until(Instant::now() - std::time::Duration::from_secs(1));
    assert!(absorb(&mut expected, &b, &expired, None).is_err());
    assert_eq!(expected, a);
}

#[test]
fn catalogue_listening_follows_the_same_rule() {
    let stats = |user: u32, recent: u32, skips: u32, skip_recent: u32, played: bool| {
        music_protocol::catalogue::sqlite::PlaybackStatistics {
            play_count_user: user,
            play_count_recent: recent,
            skip_count_user: skips,
            skip_count_recent: skip_recent,
            has_been_played: played,
            user_rating: 60,
            user_rating_is_derived: false,
            liked_state: 2,
        }
    };
    let before = stats(9, 4, 3, 1, true);
    for after in [
        stats(11, 6, 3, 1, true),
        stats(11, 2, 4, 2, true),
        stats(9, 0, 3, 0, true),
        stats(9, 4, 5, 2, true),
    ] {
        catalogue_listening(&before, &after).unwrap();
    }
    catalogue_listening(&stats(0, 0, 0, 0, false), &stats(1, 1, 0, 0, true)).unwrap();
    for after in [
        stats(8, 4, 3, 1, true),
        stats(11, 5, 3, 1, true),
        stats(9, 4, 2, 1, true),
        stats(9, 4, 3, 1, false),
    ] {
        assert!(catalogue_listening(&before, &after).is_err());
    }
    assert!(catalogue_listening(&stats(0, 0, 0, 0, false), &stats(0, 0, 0, 0, true)).is_err());
    let mut rated = before.clone();
    rated.user_rating = 80;
    assert!(catalogue_listening(&before, &rated).is_err());
    let mut liked = before.clone();
    liked.liked_state = 3;
    assert!(catalogue_listening(&before, &liked).is_err());
}
