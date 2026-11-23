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
fn statistics_rows_whose_track_is_gone_are_left_to_the_stray_rule() {
    // Items 7 and 8 have no track: since the write the phone's sanitize
    // deleted item 7's statistics row and a late write added item 8's.
    let (mut a, mut b) = cleared_then_played();
    let stray = |pid| vec![i(pid), i(0), i(0), i(0), i(0), i(0), i(0), i(0), i(0)];
    a.get_mut("item_stats").unwrap().rows.insert(stray(7), 1);
    b.get_mut("item_stats").unwrap().rows.insert(stray(8), 1);
    // The listening rule counts the tracks in the library only.
    let error = absorbed(&a, &b).unwrap_err();
    assert!(error.to_string().contains("stray rule"), "{error:#}");
    let mut expected = a.clone();
    let taken = absorb_stray_rows(&mut expected, &b, Window::LaterRun.stray_rows(false)).unwrap();
    let stats = taken.table("item_stats").unwrap();
    assert_eq!(
        (&stats.vanished, &stats.appeared),
        (&BTreeSet::from([7]), &BTreeSet::from([8]))
    );
    let (expected, report) = absorbed(&expected, &b).unwrap();
    assert_eq!(report.unwrap()["completed_events"][0]["item_pid"], 2);
    assert_eq!(expected["item_stats"], b["item_stats"]);
    // The run's own window takes none of them, or only the sweep's.
    for (transfers, vanished) in [(false, None), (true, Some(BTreeSet::from([7])))] {
        let mut expected = a.clone();
        let window = Window::SameRun.stray_rows(transfers);
        let taken = absorb_stray_rows(&mut expected, &b, window).unwrap();
        let stats = taken.table("item_stats");
        assert_eq!(stats.map(|s| s.vanished.clone()), vanished, "{window:?}");
        assert!(stats.is_none_or(|s| s.appeared.is_empty()), "{window:?}");
    }
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

/// A track deleted while it played: the player appends one more entry for
/// it as it stops, and the phone counts the history again. Track 77, removed
/// earlier, keeps its tombstone revision; its entry 101 and an older one
/// were still counted (3), and the new entry 104 names it. `count` is the after count, `entry`
/// whether 104 is appended, `tombstone` whether 77's tombstone is there.
fn deleted_while_playing(count: i64, entry: bool, tombstone: bool) -> (Tables, Tables) {
    let (mut a, mut b) = media_type_fixture();
    for (tables, count) in [(&mut a, 3), (&mut b, count)] {
        let rows = &mut tables.get_mut(MEDIA_TYPES).unwrap().rows;
        let old = rows.keys().find(|r| r[0] == i(9)).unwrap().clone();
        rows.remove(&old);
        rows.insert(vec![i(9), i(8), i(count)], 1);
        if tombstone {
            tables
                .get_mut("entity_revision")
                .unwrap()
                .rows
                .insert(vec![i(5), i(77), i(2), i(0), i(0)], 1);
        }
    }
    if entry {
        b.get_mut("container_item")
            .unwrap()
            .rows
            .insert(vec![i(104), i(9), i(77), i(2), s(""), s(""), s("")], 1);
    }
    (a, b)
}

#[test]
fn an_entry_for_a_track_deleted_while_it_played_is_taken_with_a_recount() {
    // Counted again: the two entries whose track is in the library.
    let (a, b) = deleted_while_playing(2, true, true);
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(expected[MEDIA_TYPES], b[MEDIA_TYPES]);
    let report = report.unwrap();
    assert_eq!(
        report["history_media_type_counts"],
        json!([{"media_type":8,"before":3,"after":2,"counted_again":true}])
    );
    assert_eq!(report["playback_history"]["appended"][1]["item_pid"], 77);
    assert_eq!(
        report["playback_history"]["appended"][1]["track_deleted"],
        true
    );
    // Or by the change in such entries, as without the deleted track's entry.
    absorbed(
        &deleted_while_playing(4, true, true).0,
        &deleted_while_playing(4, true, true).1,
    )
    .unwrap();
    for (case, count, entry, tombstone) in [
        (
            "an entry naming a track the phone never deleted",
            2,
            true,
            false,
        ),
        (
            "a recount without an entry for a deleted track",
            2,
            false,
            true,
        ),
        ("a count that is neither", 5, true, true),
    ] {
        let (a, b) = deleted_while_playing(count, entry, tombstone);
        assert!(absorbed(&a, &b).is_err(), "{case}");
    }
}

/// A removal of every track while one played (a clear): the model holds no
/// track; 1, 2 and 77 keep tombstones. The player appends 103 for track 1,
/// deleted as it played; the phone counts the history again, finds no entry
/// for a track in the library, drops its media type row and sets its
/// `contained_media_type` to 0. `live` keeps track 2 in the library.
fn emptied_by_a_clear(live: bool, appended: bool) -> (Tables, Tables) {
    let (mut a, mut b) = deleted_while_playing(2, false, true);
    for tables in [&mut a, &mut b] {
        let rows = match live {
            true => vec![vec![i(2), i(8)]],
            false => vec![],
        };
        tables.insert("item".into(), table(&["item_pid", "media_type"], rows));
        tables.get_mut("item_stats").unwrap().rows.clear();
        let revisions = &mut tables.get_mut("entity_revision").unwrap().rows;
        revisions.retain(|r, _| r[1] == i(9) || r[1] == i(77));
        revisions.insert(vec![i(6), i(1), i(2), i(0), i(0)], 1);
        if !live {
            revisions.insert(vec![i(7), i(2), i(2), i(0), i(0)], 1);
        }
    }
    b.get_mut(MEDIA_TYPES)
        .unwrap()
        .rows
        .retain(|r, _| r[0] != i(9));
    change(
        &mut b,
        "container",
        "container_pid",
        9,
        "contained_media_type",
        i(0),
    );
    if !appended {
        b.insert("container_item".into(), a["container_item"].clone());
    }
    (a, b)
}

#[test]
fn a_clear_while_a_track_played_empties_the_history_count() {
    let (a, b) = emptied_by_a_clear(false, true);
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(expected["container"], b["container"]);
    assert_eq!(expected[MEDIA_TYPES], b[MEDIA_TYPES]);
    let report = report.unwrap();
    assert_eq!(
        report["history_media_type_counts"],
        json!([{"media_type":8,"before":3,"after":null,"counted_again":true}])
    );
    assert_eq!(
        report["playback_history"]["contained_media_type"],
        json!({"before":8,"after":0})
    );
    for (case, live, appended) in [
        ("an entry still names a track in the library", true, true),
        ("no entry for a deleted track", false, false),
    ] {
        let (a, b) = emptied_by_a_clear(live, appended);
        assert!(absorbed(&a, &b).is_err(), "{case}");
    }
}

/// The history a clear emptied (`contained_media_type` 0, no media type
/// row) when the library is back and a song plays: the phone sets it to 8
/// and its row counts the new entries for tracks in the library from none.
/// Entry 102 names track 2, back in the library under a reused id; the new
/// entry 103 names track 1, or (`live` false) track 77, deleted earlier.
fn filled_after_a_clear(count: i64, contained: i64, live: bool) -> (Tables, Tables) {
    let (mut a, mut b) = media_type_fixture();
    for tables in [&mut a, &mut b] {
        tables
            .get_mut(MEDIA_TYPES)
            .unwrap()
            .rows
            .retain(|r, _| r[0] != i(9));
        tables
            .get_mut("entity_revision")
            .unwrap()
            .rows
            .insert(vec![i(5), i(77), i(2), i(0), i(0)], 1);
    }
    change(
        &mut a,
        "container",
        "container_pid",
        9,
        "contained_media_type",
        i(0),
    );
    change(
        &mut b,
        "container",
        "container_pid",
        9,
        "contained_media_type",
        i(contained),
    );
    b.get_mut(MEDIA_TYPES)
        .unwrap()
        .rows
        .insert(vec![i(9), i(8), i(count)], 1);
    if !live {
        change(
            &mut b,
            "container_item",
            "container_item_pid",
            103,
            "item_pid",
            i(77),
        );
    }
    (a, b)
}

#[test]
fn the_history_a_clear_emptied_counts_again_from_none() {
    let (a, b) = filled_after_a_clear(1, 8, true);
    let (expected, report) = absorbed(&a, &b).unwrap();
    assert_eq!(expected["container"], b["container"]);
    assert_eq!(expected[MEDIA_TYPES], b[MEDIA_TYPES]);
    let report = report.unwrap();
    assert_eq!(
        report["history_media_type_counts"],
        json!([{"media_type":8,"before":null,"after":1}])
    );
    assert_eq!(
        report["playback_history"]["contained_media_type"],
        json!({"before":0,"after":8})
    );
    for (case, count, contained, live) in [
        ("a row counting more than the new entries", 2, 8, true),
        ("another media type", 1, 1, true),
        ("no new entry for a track in the library", 1, 8, false),
    ] {
        let (a, b) = filled_after_a_clear(count, contained, live);
        assert!(absorbed(&a, &b).is_err(), "{case}");
    }
}

#[test]
fn changes_listening_cannot_explain_are_refused() {
    type Edit = Box<dyn Fn(&mut Tables)>;
    let cases: Vec<(&str, Edit)> = vec![
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

/// A run's own check takes the exact model first and the later-run rule only
/// when that refuses, over the same captures; the window that passed comes
/// back. A later run's check is the later-run rule alone.
#[test]
fn a_runs_own_check_falls_back_to_the_later_run_rule() {
    let mut asked = Vec::new();
    let check = |exact: bool, later: bool| {
        move |window: Window| -> Result<Window> {
            match window {
                Window::SameRun if exact => Ok(window),
                Window::LaterRun if later => Ok(window),
                _ => anyhow::bail!("{window:?} refused"),
            }
        }
    };
    let mut record = |window: Window, exact: bool, later: bool| {
        let inner = check(exact, later);
        let mut seen = Vec::new();
        let result = window.check(|w| {
            seen.push(w);
            inner(w)
        });
        asked.push(seen);
        result
    };
    assert_eq!(
        record(Window::SameRun, true, true).unwrap(),
        (Window::SameRun, Window::SameRun)
    );
    assert_eq!(
        record(Window::SameRun, false, true).unwrap(),
        (Window::LaterRun, Window::LaterRun)
    );
    let refused = record(Window::SameRun, false, false).unwrap_err();
    assert_eq!(refused.to_string(), "LaterRun refused");
    assert!(record(Window::LaterRun, true, false).is_err());
    assert_eq!(
        asked,
        [
            vec![Window::SameRun],
            vec![Window::SameRun, Window::LaterRun],
            vec![Window::SameRun, Window::LaterRun],
            vec![Window::LaterRun],
        ]
    );
}
