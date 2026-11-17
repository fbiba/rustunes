//! The stray rule on the native fixture, whose after state lost track 1.
use super::*;
use crate::catalogue::removal::tests::tables;
use crate::test_support::catalogue::{NativeFixture, SANITIZED, STRAY_ROWS as STRAY};
use rusqlite::Connection;
use std::{fs, os::unix::fs::PermissionsExt};

/// Item 9's stray rows (`STRAY`) in both states, then `after` on the after
/// state.
fn pair(after: &str) -> (Tables, Tables) {
    pair_with("", after)
}

/// `pair`, with `both` run on both states after `STRAY`.
fn pair_with(both: &str, after: &str) -> (Tables, Tables) {
    let fixture = NativeFixture::new();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    for path in [&fixture.before, &fixture.after] {
        let c = Connection::open(path).unwrap();
        c.execute_batch(STRAY).unwrap();
        c.execute_batch(both).unwrap();
    }
    Connection::open(&fixture.after)
        .unwrap()
        .execute_batch(after)
        .unwrap();
    let pair = (
        tables(&fixture.before, &fixture.dir.join("b")),
        tables(&fixture.after, &fixture.dir.join("a")),
    );
    fs::remove_dir_all(&fixture.dir).unwrap();
    pair
}

fn at(tables: &Tables, name: &str, pid: i64) -> Vec<Vec<Cell>> {
    let t = &tables[name];
    let key = t.column("item_pid").unwrap();
    t.rows
        .keys()
        .filter(|r| r[key] == Cell::Integer(pid))
        .cloned()
        .collect()
}

#[test]
fn each_window_takes_only_what_the_phone_may_have_done() {
    let (before, after) = pair(&format!(
        "{SANITIZED} INSERT OR REPLACE INTO item_search(item_pid,search_title)VALUES(7,3);"
    ));
    for (window, vanished, appeared) in [
        (StrayWindow::WithoutTransfers, &[][..], &[][..]),
        (
            StrayWindow::WithTransfers,
            &["item_stats", "item_store"][..],
            &[][..],
        ),
        (
            StrayWindow::BetweenRuns,
            &["item_extra", "item_stats", "item_store"][..],
            &["item_search"][..],
        ),
    ] {
        let mut expected = before.clone();
        let taken = absorb_stray_rows(&mut expected, &after, window).unwrap();
        for name in STRAY_TABLES {
            let changes = taken.table(name).cloned().unwrap_or_default();
            let gone = vanished.contains(&name);
            let added = appeared.contains(&name);
            assert_eq!(changes.vanished.contains(&9), gone, "{window:?} {name}");
            assert_eq!(changes.appeared.contains(&7), added, "{window:?} {name}");
            // Item 9's rows follow the after state where taken, and the
            // before state elsewhere.
            let source = if gone { &after } else { &before };
            assert_eq!(at(&expected, name, 9), at(source, name, 9), "{name}");
            let source = if added { &after } else { &before };
            assert_eq!(at(&expected, name, 7), at(source, name, 7), "{name}");
            // Track 1, which the after state lost, is no stray row.
            assert_eq!(at(&expected, name, 1), at(&before, name, 1), "{name}");
        }
        assert_eq!(expected["item"], before["item"]);
    }
    // The one change no window takes: a stray item_state row that is gone.
    let (before, after) = pair("DELETE FROM item_state WHERE item_pid=9");
    let mut expected = before.clone();
    let taken = absorb_stray_rows(&mut expected, &after, StrayWindow::BetweenRuns).unwrap();
    assert!(taken.is_empty());
    assert_eq!(expected, before);
}

#[test]
fn a_new_track_replaces_the_stray_rows_at_its_slot() {
    let (before, _) = pair("");
    let mut expected = before.clone();
    let taken = replace_stray_rows(&mut expected, 9).unwrap();
    for name in STRAY_TABLES {
        let written = ["item_extra", "item_store", "item_stats", "item_state"].contains(&name);
        assert_eq!(
            taken
                .table(name)
                .is_some_and(|t| t.replaced == BTreeSet::from([9])),
            written,
            "{name}"
        );
        assert!(at(&expected, name, 9).is_empty(), "{name}");
        assert_eq!(
            expected[name].rows.len() + usize::from(written),
            before[name].rows.len()
        );
    }
    // A live track's pid is never a slot.
    assert!(replace_stray_rows(&mut expected, 2).is_err());
}

#[test]
fn a_sync_id_names_a_track_never_a_row_whose_track_is_gone() {
    // Item 9's store row holds track 2's sync id 1002, and item 8's 1008.
    let (before, _) = pair_with(
        "UPDATE item_store SET sync_id=1002 WHERE item_pid=9;
        INSERT OR REPLACE INTO item_store(item_pid,sync_id)VALUES(8,1008);",
        "",
    );
    assert_eq!(track_with_sync_id(&before, 1002).unwrap(), 2);
    assert!(track_with_sync_id(&before, 1008).is_err());
    assert!(track_with_sync_id(&before, 1009).is_err());
}
