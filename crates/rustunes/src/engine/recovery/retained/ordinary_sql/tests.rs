//! The insert model's handling of rows whose track is gone, on the native
//! fixture: tracks 1 to 4, so a new track takes slot 5.
use super::*;
use crate::engine::verification::sql_tables::tests::Fixture;
use music_protocol::test_support::catalogue::STRAY_ROWS;
use rusqlite::{Connection, params_from_iter, types::Value};
use std::{fs, os::unix::fs::PermissionsExt};

/// The library container, which the operation names by its sync id.
const LIBRARY: &str =
    "INSERT INTO container(container_pid,sync_id,name,is_hidden)VALUES(-3,-3,'Library',1);";

/// Item `pid`'s rows as a property write for a deleted track leaves them,
/// with the item_state row the fixture's triggers add.
fn stray(pid: i64) -> String {
    STRAY_ROWS.replace("VALUES(9,", &format!("VALUES({pid},"))
}

fn snapshot(fixture: &Fixture, database: &Path, name: &str) -> Snapshot {
    let capture = Capture {
        database,
        database_sha256: Sha256::digest(fs::read(database).unwrap()).into(),
        wal: None,
    };
    reader::materialize(capture, &fixture.dir.join(name), &mut Budget::new()).unwrap()
}

/// The fixture's before state after `both`, and a copy of it after `after`.
fn pair(both: &str, after: &str) -> (Fixture, Snapshot, Snapshot) {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    Connection::open(&fixture.before)
        .unwrap()
        .execute_batch(both)
        .unwrap();
    fs::copy(&fixture.before, &fixture.after).unwrap();
    Connection::open(&fixture.after)
        .unwrap()
        .execute_batch(after)
        .unwrap();
    let a = snapshot(&fixture, &fixture.before, "a");
    let z = snapshot(&fixture, &fixture.after, "z");
    (fixture, a, z)
}

#[test]
fn the_new_slot_may_hold_rows_whose_track_is_gone_and_no_other_reference() {
    let both = format!("{LIBRARY}{}", stray(5));
    let (_f, a, _) = pair(&both, "");
    assert!(a.table("item_state").unwrap().one("item_pid", 5).is_ok());
    assert_eq!(
        allocation_facts(&a, -2, -3, 10, &Budget::new())
            .unwrap()
            .slot,
        5
    );
    for sql in [
        "INSERT INTO container_item(container_item_pid,container_pid,item_pid,position)VALUES(500,-3,5,0)",
        "INSERT INTO chapter(item_pid)VALUES(5)",
        "UPDATE album SET representative_item_pid=5 WHERE album_pid=11",
    ] {
        let (_f, a, _) = pair(&format!("{both}{sql};"), "");
        let Err(error) = allocation_facts(&a, -2, -3, 10, &Budget::new()) else {
            panic!("{sql}: the slot was accepted");
        };
        assert!(
            error
                .to_string()
                .contains("new internal slot still referenced"),
            "{sql}: {error:#}"
        );
    }
}

#[test]
fn a_destination_named_only_by_a_row_whose_track_is_gone_is_free() {
    // Item 9's row names T9.m4a; tracks 1 to 4 have T1.m4a to T4.m4a in
    // base location 1.
    let (_f, a, _) = pair(&stray(9), "");
    assert_eq!(
        destination(&a, "iTunes_Control/Music/F00/t9.M4A").unwrap(),
        1
    );
    let error = destination(&a, "iTunes_Control/Music/F00/T2.m4a").unwrap_err();
    assert!(error.to_string().contains("occupied"), "{error:#}");
}

#[test]
fn recent_counter_resets_pass_over_statistics_rows_whose_track_is_gone() {
    // The session cleared the tracks' recent counters, and the sweep ending
    // it deleted item 9's store and statistics rows.
    let (_f, a, z) = pair(
        &stray(9),
        "UPDATE item_stats SET play_count_recent=0,skip_count_recent=0 WHERE item_pid<5;
        DELETE FROM item_store WHERE item_pid=9; DELETE FROM item_stats WHERE item_pid=9;",
    );
    let mut expected = a.tables.clone();
    let resets = model::reset_recent(&a, &z, &mut expected, &Budget::new()).unwrap();
    let reset: BTreeSet<i64> = resets
        .iter()
        .map(|r| r["item_pid"].as_i64().unwrap())
        .collect();
    assert_eq!(reset, BTreeSet::from([1, 2, 3]));
    let taken =
        absorb_stray_rows(&mut expected, &z.tables, Window::SameRun.stray_rows(true)).unwrap();
    for name in ["item_store", "item_stats"] {
        assert_eq!(
            taken.table(name).unwrap().vanished,
            BTreeSet::from([9]),
            "{name}"
        );
    }
    assert_eq!(expected, z.tables);
}

fn wire() -> Wire {
    Wire {
        title: "New".into(),
        album: "New Album".into(),
        artist: "New Artist".into(),
        album_artist: "New Artist".into(),
        genre: None,
        copyright: None,
        year: 2020,
        track: 1,
        tracks: 1,
        disc: 1,
        discs: 1,
        millis: 1000,
        ticks: 44100,
        timestamp: 1_700_000_000.0,
        database: -2,
        primary: -3,
        album_wire: 7_000_000_002,
        artist_wire: 7_000_000_003,
    }
}

fn sql_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Integer(n) => Value::Integer(*n),
        Cell::Real(bits) => Value::Real(f64::from_bits(*bits)),
        Cell::Text(v) => Value::Text(v.clone()),
        Cell::Blob(v) => Value::Blob(v.clone()),
    }
}

/// The new track's import into `database`: `rows` written over each
/// table's row at slot 5, item_state built by the triggers (over a row
/// already there), then the item_state columns in `own` set to their
/// modelled values.
fn import(database: &Path, a: &Snapshot, rows: &BTreeMap<&str, Row>, own: &[String]) {
    let c = Connection::open(database).unwrap();
    for name in [
        "item_store",
        "item_extra",
        "item_stats",
        "item_playback",
        "item_search",
        "item_video",
        "chapter",
        "lyrics",
        "item",
    ] {
        let columns: Vec<_> = a
            .table(name)
            .unwrap()
            .columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect();
        let marks = vec!["?"; columns.len()].join(",");
        c.execute(
            &format!(
                "INSERT OR REPLACE INTO {name}({})VALUES({marks})",
                columns.join(",")
            ),
            params_from_iter(rows[name].iter().map(sql_value)),
        )
        .unwrap();
    }
    let state = a.table("item_state").unwrap();
    for column in own {
        let value = sql_value(&rows["item_state"][state.column(column).unwrap()]);
        c.execute(
            &format!("UPDATE item_state SET \"{column}\"=?1 WHERE item_pid=5"),
            [value],
        )
        .unwrap();
    }
}

#[test]
fn a_new_track_at_the_slot_replaces_the_rows_whose_track_is_gone_there() {
    // Items 5 and 9 have no track; the new track takes slot 5.
    let (fixture, a, _) = pair(&format!("{}{}", stray(5), stray(9)), "");
    let integrity: Vec<u8> = [4, 0].into_iter().chain([7; 55]).collect();
    let rows = rows::new_rows(
        &a,
        &wire(),
        rows::NewValues {
            id: 5,
            groups: [100, 101, 102],
            base: 1,
            file: "NEW.m4a",
            track: 7_000_000_001,
            bytes: 4096,
            maps: [(500_001, 12), (500_002, 12), (500_003, 12)],
            // Fractional, as the phone's are: an integral real in an
            // INTEGER column is stored as an integer.
            dates: [700_000_000.25, 700_000_000.25],
            integrity: &integrity,
        },
    )
    .unwrap();
    // The item_state columns the import sets itself: those its triggers
    // leave unlike the modelled row when slot 5 holds no row.
    let (fresh, clear, _) = pair(&stray(9), "");
    import(&fresh.after, &clear, &rows, &[]);
    let built = snapshot(&fresh, &fresh.after, "built");
    let state = built.table("item_state").unwrap();
    let row = state.one("item_pid", 5).unwrap();
    let own: Vec<String> = (0..row.len())
        .filter(|j| row[*j] != rows["item_state"][*j])
        .map(|j| state.columns[j].clone())
        .collect();
    assert!(!own.is_empty());
    // Over item 5's rows the import's triggers take over its item_state row,
    // which ends as the model's; the sweep ending the session then deletes
    // item 9's store and statistics rows.
    import(&fixture.after, &a, &rows, &own);
    Connection::open(&fixture.after)
        .unwrap()
        .execute_batch(
            "DELETE FROM item_store WHERE item_pid=9; DELETE FROM item_stats WHERE item_pid=9;",
        )
        .unwrap();
    let z = snapshot(&fixture, &fixture.after, "imported");
    assert_eq!(
        z.table("item_state").unwrap().one("item_pid", 5).unwrap(),
        &rows["item_state"]
    );
    // The model, in the run's own window and in a later run's; a session
    // without transfers would not have swept.
    for (window, transfers, equal) in [
        (Window::SameRun, true, true),
        (Window::LaterRun, true, true),
        (Window::SameRun, false, false),
    ] {
        let mut expected = a.tables.clone();
        let mut taken = replace_stray_rows(&mut expected, 5).unwrap();
        for (name, row) in rows.clone() {
            model::add(&mut expected, name, row).unwrap();
        }
        taken.extend(
            absorb_stray_rows(&mut expected, &z.tables, window.stray_rows(transfers)).unwrap(),
        );
        assert_eq!(expected == z.tables, equal, "{window:?} {transfers}");
        if !equal {
            continue;
        }
        for name in ["item_extra", "item_store", "item_stats", "item_state"] {
            let changes = taken.table(name).unwrap();
            assert_eq!(changes.replaced, BTreeSet::from([5]), "{name}");
            let swept = ["item_store", "item_stats"].contains(&name);
            assert_eq!(changes.vanished.contains(&9), swept, "{name}");
        }
    }
    // Without replacing them, the model keeps item 5's rows beside the new
    // track's.
    let mut expected = a.tables.clone();
    for (name, row) in rows {
        model::add(&mut expected, name, row).unwrap();
    }
    absorb_stray_rows(&mut expected, &z.tables, Window::SameRun.stray_rows(true)).unwrap();
    assert_ne!(expected["item_extra"], z.tables["item_extra"]);
}
