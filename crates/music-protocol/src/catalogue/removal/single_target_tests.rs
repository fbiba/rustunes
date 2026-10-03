//! Typed-table fixtures: target item 1 with its own album/artists, survivor 2
//! sharing genre 40 (whose representative is the target). Synthetic
//! single-target cases, carried over from the model of removing an interrupted
//! insert's row.
use super::*;

fn i(n: i64) -> Cell {
    Cell::Integer(n)
}
fn s(v: &str) -> Cell {
    Cell::Text(v.into())
}
fn table_of(columns: &[&str], rows: Vec<Vec<Cell>>) -> Table {
    let mut result = Rows::new();
    for row in rows {
        *result.entry(row).or_default() += 1;
    }
    Table {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        definitions: Vec::new(),
        rows: result,
    }
}

fn before() -> Tables {
    let mut t = Tables::new();
    t.insert(
        "item".into(),
        table_of(
            &[
                "item_pid",
                "album_pid",
                "album_artist_pid",
                "item_artist_pid",
                "genre_id",
                "composer_pid",
            ],
            vec![
                vec![i(1), i(10), i(20), i(30), i(40), i(0)],
                vec![i(2), i(11), i(21), i(31), i(40), i(0)],
            ],
        ),
    );
    for name in ITEM_TABLES
        .iter()
        .filter(|n| !matches!(**n, "item" | "item_stats"))
    {
        t.insert(
            (*name).into(),
            table_of(
                &["item_pid", "value"],
                vec![vec![i(1), s("own")], vec![i(2), s("survivor")]],
            ),
        );
    }
    t.insert(
        "item_stats".into(),
        table_of(
            &[
                "item_pid",
                "play_count_recent",
                "skip_count_recent",
                "play_count",
            ],
            vec![vec![i(1), i(0), i(0), i(0)], vec![i(2), i(3), i(1), i(9)]],
        ),
    );
    for (name, key, rows) in [
        ("album", "album_pid", vec![(10, 1), (11, 2)]),
        ("album_artist", "album_artist_pid", vec![(20, 1), (21, 2)]),
        ("item_artist", "item_artist_pid", vec![(30, 1), (31, 2)]),
        ("genre", "genre_id", vec![(40, 1)]),
        ("composer", "composer_pid", vec![]),
    ] {
        t.insert(
            name.into(),
            table_of(
                &[key, "representative_item_pid", "name"],
                rows.into_iter()
                    .map(|(id, rep)| vec![i(id), i(rep), s(name)])
                    .collect(),
            ),
        );
    }
    t.insert(
        "entity_revision".into(),
        table_of(
            &[
                "entity_pid",
                "class",
                "revision_type",
                "revision",
                "deleted",
            ],
            vec![
                vec![i(1), i(0), i(0), i(5), i(0)],
                vec![i(1), i(0), i(1), i(4), i(0)],
                vec![i(2), i(0), i(0), i(3), i(0)],
                vec![i(10), i(1), i(0), i(6), i(0)],
            ],
        ),
    );
    t.insert(
        "_MLDatabaseProperties".into(),
        table_of(
            &["key", "value"],
            vec![
                vec![s(ANCHOR), s("140")],
                vec![s(LAST_SYNCED), s("6")],
                vec![s(GENERATION), s("-7")],
                vec![s(HERO), s("6")],
                vec![s("Other"), s("x")],
            ],
        ),
    );
    for name in ["artwork_token", "best_artwork_token"] {
        t.insert(
            name.into(),
            table_of(&["entity_pid", "entity_type", "token"], Vec::new()),
        );
    }
    t.insert(
        "container_item".into(),
        table_of(&["item_pid", "container_pid"], vec![vec![i(2), i(99)]]),
    );
    t
}

fn set_cell(t: &mut Tables, name: &str, key: &str, id: i64, column: &str, value: Cell) {
    let table = t.get_mut(name).unwrap();
    let old = table.one(key, id).unwrap().clone();
    let mut new = old.clone();
    new[table.column(column).unwrap()] = value;
    table.rows.remove(&old);
    table.rows.insert(new, 1);
}
fn set_property(t: &mut Tables, key: &str, value: &str) {
    let table = t.get_mut("_MLDatabaseProperties").unwrap();
    table.rows.retain(|r, _| r[0] != s(key));
    table.rows.insert(vec![s(key), s(value)], 1);
}

/// The ordinary last-collection outcome of removing item 1 at anchor 140.
fn after() -> Tables {
    let mut t = before();
    for name in ITEM_TABLES {
        t.get_mut(name).unwrap().rows.retain(|r, _| r[0] != i(1));
    }
    set_cell(
        &mut t,
        "item_stats",
        "item_pid",
        2,
        "play_count_recent",
        i(0),
    );
    set_cell(
        &mut t,
        "item_stats",
        "item_pid",
        2,
        "skip_count_recent",
        i(0),
    );
    let revisions = t.get_mut("entity_revision").unwrap();
    revisions.rows.remove(&vec![i(1), i(0), i(0), i(5), i(0)]);
    revisions.rows.remove(&vec![i(1), i(0), i(1), i(4), i(0)]);
    revisions.rows.insert(vec![i(1), i(0), i(0), i(7), i(2)], 1);
    set_property(&mut t, ANCHOR, "141");
    set_property(&mut t, LAST_SYNCED, "7");
    set_property(&mut t, GENERATION, "123");
    for (name, key, id) in [
        ("album", "album_pid", 10),
        ("album_artist", "album_artist_pid", 20),
        ("item_artist", "item_artist_pid", 30),
    ] {
        set_cell(&mut t, name, key, id, "representative_item_pid", i(0));
    }
    set_cell(
        &mut t,
        "genre",
        "genre_id",
        40,
        "representative_item_pid",
        i(2),
    );
    t
}

#[test]
fn ordinary_last_collection_removal_is_accepted_and_recorded() {
    let receipt = verify(&before(), &after(), &[1], 140).unwrap();
    assert_eq!(receipt.targets[0].removed_rows.len(), 10);
    assert_eq!(
        (
            receipt.targets[0].tombstone_revision_before,
            receipt.targets[0].tombstone_revision_after
        ),
        (5, 7)
    );
    assert_eq!(receipt.targets[0].removed_stats_revision, Some(4));
    assert_eq!(receipt.recent_counter_resets, [2]);
    assert_eq!(receipt.representatives.len(), 4);
    assert_eq!(receipt.property_changes.len(), 3);
    assert!(receipt.survivor_revision_refresh.is_empty());
}

#[test]
fn retained_stale_representatives_and_counters_are_accepted() {
    let mut z = after();
    set_cell(
        &mut z,
        "album",
        "album_pid",
        10,
        "representative_item_pid",
        i(1),
    );
    set_cell(
        &mut z,
        "genre",
        "genre_id",
        40,
        "representative_item_pid",
        i(1),
    );
    set_cell(
        &mut z,
        "item_stats",
        "item_pid",
        2,
        "play_count_recent",
        i(3),
    );
    set_cell(
        &mut z,
        "item_stats",
        "item_pid",
        2,
        "skip_count_recent",
        i(1),
    );
    let receipt = verify(&before(), &z, &[1], 140).unwrap();
    assert!(receipt.recent_counter_resets.is_empty());
    assert_eq!(receipt.representatives.len(), 2);
}

#[test]
fn group_removal_or_foreign_representatives_refuse() {
    let mut removed = after();
    removed
        .get_mut("album")
        .unwrap()
        .rows
        .retain(|r, _| r[0] != i(10));
    assert!(verify(&before(), &removed, &[1], 140).is_err());
    // The emptied album cannot point at a member of another album.
    let mut foreign = after();
    set_cell(
        &mut foreign,
        "album",
        "album_pid",
        10,
        "representative_item_pid",
        i(2),
    );
    assert!(verify(&before(), &foreign, &[1], 140).is_err());
    let mut renamed = after();
    set_cell(&mut renamed, "album", "album_pid", 10, "name", s("other"));
    assert!(verify(&before(), &renamed, &[1], 140).is_err());
}

#[test]
fn survivor_target_or_property_drift_refuses() {
    let mut survivor = after();
    set_cell(
        &mut survivor,
        "item_extra",
        "item_pid",
        2,
        "value",
        s("changed"),
    );
    assert!(verify(&before(), &survivor, &[1], 140).is_err());
    let mut partial = after();
    partial
        .get_mut("item_search")
        .unwrap()
        .rows
        .insert(vec![i(1), s("own")], 1);
    assert!(verify(&before(), &partial, &[1], 140).is_err());
    let mut skipped = after();
    set_property(&mut skipped, ANCHOR, "142");
    assert!(verify(&before(), &skipped, &[1], 140).is_err());
    let mut other = after();
    set_property(&mut other, "Other", "y");
    assert!(verify(&before(), &other, &[1], 140).is_err());
    let mut live = after();
    let revisions = live.get_mut("entity_revision").unwrap();
    revisions.rows.remove(&vec![i(1), i(0), i(0), i(7), i(2)]);
    revisions.rows.insert(vec![i(1), i(0), i(0), i(7), i(0)], 1);
    assert!(verify(&before(), &live, &[1], 140).is_err());
    let mut playlist = after();
    playlist.get_mut("container_item").unwrap().rows.clear();
    assert!(verify(&before(), &playlist, &[1], 140).is_err());
}

#[test]
fn unmodelled_target_relationships_refuse_before_sending() {
    let mut t = before();
    t.get_mut("container_item")
        .unwrap()
        .rows
        .insert(vec![i(1), i(99)], 1);
    assert!(qualify(&t, &[1]).is_err());
    assert!(qualify(&before(), &[1]).is_ok());
    assert!(qualify(&before(), &[3]).is_err());
}

#[test]
fn survivor_revision_refresh_must_be_contiguous_above_tombstone() {
    let refreshed = |revision| {
        let mut z = after();
        let revisions = z.get_mut("entity_revision").unwrap();
        revisions.rows.remove(&vec![i(2), i(0), i(0), i(3), i(0)]);
        revisions
            .rows
            .insert(vec![i(2), i(0), i(0), i(revision), i(0)], 1);
        z
    };
    let receipt = verify(&before(), &refreshed(8), &[1], 140).unwrap();
    assert_eq!(
        receipt.survivor_revision_refresh,
        [RevisionRefresh {
            item_pid: 2,
            before: 3,
            after: 8
        }]
    );
    assert!(verify(&before(), &refreshed(9), &[1], 140).is_err());
}

#[test]
fn maintenance_clock_may_only_advance() {
    let mut a = before();
    set_property(&mut a, MAINTENANCE, "749928000.5");
    let mut z = after();
    set_property(&mut z, MAINTENANCE, "749928060.25");
    let receipt = verify(&a, &z, &[1], 140).unwrap();
    assert_eq!(receipt.property_changes.len(), 4);
    set_property(&mut z, MAINTENANCE, "749927999");
    assert!(verify(&a, &z, &[1], 140).is_err());
    set_property(&mut z, MAINTENANCE, "-1");
    assert!(verify(&a, &z, &[1], 140).is_err());
    // The clock cannot disappear or appear.
    assert!(verify(&a, &after(), &[1], 140).is_err());
    let mut added = after();
    set_property(&mut added, MAINTENANCE, "749928060");
    assert!(verify(&before(), &added, &[1], 140).is_err());
}

/// Album 10 is emptied by removing item 1; album 11 keeps survivor 2. Both
/// carry keep_local_status and a class-4 album revision.
fn with_album_status(mut t: Tables) -> Tables {
    let album = t.get_mut("album").unwrap();
    album.columns.push("keep_local_status".into());
    album.rows = album
        .rows
        .iter()
        .map(|(r, n)| {
            let mut r = r.clone();
            r.push(i(0));
            (r, *n)
        })
        .collect();
    let revisions = t.get_mut("entity_revision").unwrap();
    revisions
        .rows
        .insert(vec![i(10), i(4), i(0), i(6), i(0)], 1);
    revisions
        .rows
        .insert(vec![i(11), i(4), i(0), i(2), i(0)], 1);
    t
}
fn bump_album(t: &mut Tables, album: i64, from: i64, to: i64) {
    let revisions = t.get_mut("entity_revision").unwrap();
    assert_eq!(
        revisions
            .rows
            .remove(&vec![i(album), i(4), i(0), i(from), i(0)]),
        Some(1)
    );
    revisions
        .rows
        .insert(vec![i(album), i(4), i(0), i(to), i(0)], 1);
}

#[test]
fn emptied_album_may_later_take_status_two_and_the_next_revision() {
    let a = with_album_status(before());
    let z = with_album_status(after());
    assert!(verify(&a, &z, &[1], 140).unwrap().emptied_albums.is_empty());

    let mut later = z.clone();
    set_cell(
        &mut later,
        "album",
        "album_pid",
        10,
        "keep_local_status",
        i(2),
    );
    bump_album(&mut later, 10, 6, 8);
    let receipt = verify(&a, &later, &[1], 140).unwrap();
    assert_eq!(
        receipt.emptied_albums,
        [EmptiedAlbum {
            album_pid: 10,
            keep_local_status: Some((0, 2)),
            revision: Some((6, 8)),
            removed_stats_revision: None,
        }]
    );

    // Together with a survivor refresh the new revisions stay contiguous.
    let mut both = later.clone();
    let revisions = both.get_mut("entity_revision").unwrap();
    revisions.rows.remove(&vec![i(2), i(0), i(0), i(3), i(0)]);
    revisions.rows.insert(vec![i(2), i(0), i(0), i(9), i(0)], 1);
    assert_eq!(
        verify(&a, &both, &[1], 140)
            .unwrap()
            .survivor_revision_refresh
            .len(),
        1
    );
}

/// Seen on the phone: an emptied album whose revision moved also lost its stats
/// revision row. Without that move the row stays.
#[test]
fn an_emptied_album_may_lose_its_stats_revision_as_its_revision_moves() {
    let stats_row = || vec![i(10), i(4), i(1), i(1), i(0)];
    let with_stats = |mut t: Tables| {
        t.get_mut("entity_revision")
            .unwrap()
            .rows
            .insert(stats_row(), 1);
        t
    };
    let a = with_stats(with_album_status(before()));
    let z = with_stats(with_album_status(after()));
    assert!(verify(&a, &z, &[1], 140).unwrap().emptied_albums.is_empty());

    let mut later = z.clone();
    set_cell(
        &mut later,
        "album",
        "album_pid",
        10,
        "keep_local_status",
        i(2),
    );
    bump_album(&mut later, 10, 6, 8);
    let kept = verify(&a, &later, &[1], 140).unwrap();
    assert_eq!(kept.emptied_albums[0].removed_stats_revision, None);
    let revisions = later.get_mut("entity_revision").unwrap();
    assert_eq!(revisions.rows.remove(&stats_row()), Some(1));
    assert_eq!(
        verify(&a, &later, &[1], 140).unwrap().emptied_albums,
        [EmptiedAlbum {
            album_pid: 10,
            keep_local_status: Some((0, 2)),
            revision: Some((6, 8)),
            removed_stats_revision: Some(1),
        }]
    );

    let mut alone = z.clone();
    let revisions = alone.get_mut("entity_revision").unwrap();
    assert_eq!(revisions.rows.remove(&stats_row()), Some(1));
    assert!(verify(&a, &alone, &[1], 140).is_err());
}

#[test]
fn emptied_album_changes_beyond_the_observed_shape_refuse() {
    let a = with_album_status(before());
    let z = with_album_status(after());
    let refused = |edit: &dyn Fn(&mut Tables)| {
        let mut t = z.clone();
        edit(&mut t);
        assert!(verify(&a, &t, &[1], 140).is_err());
    };
    // Another status value, or another cell alongside it.
    refused(&|t| set_cell(t, "album", "album_pid", 10, "keep_local_status", i(1)));
    refused(&|t| {
        set_cell(t, "album", "album_pid", 10, "keep_local_status", i(2));
        set_cell(t, "album", "album_pid", 10, "name", s("other"));
    });
    // An album that still has a survivor is not emptied.
    refused(&|t| set_cell(t, "album", "album_pid", 11, "keep_local_status", i(2)));
    refused(&|t| bump_album(t, 11, 2, 8));
    // The album revision must sit directly above the tombstone, alone or
    // together with a survivor refresh, without collisions or gaps.
    refused(&|t| bump_album(t, 10, 6, 9));
    refused(&|t| bump_album(t, 10, 6, 7));
    refused(&|t| {
        bump_album(t, 10, 6, 8);
        let revisions = t.get_mut("entity_revision").unwrap();
        revisions.rows.remove(&vec![i(2), i(0), i(0), i(3), i(0)]);
        revisions.rows.insert(vec![i(2), i(0), i(0), i(8), i(0)], 1);
    });
}
