//! Full native schema fixtures for deleting empty playlists.
use super::*;
use crate::catalogue::removal::tests::tables;
use crate::test_support::catalogue::NativeFixture as Fixture;
use rusqlite::Connection;
use std::{fs, os::unix::fs::PermissionsExt};

/// Library container -3, Playback History 77, the empty ordinary playlists 88
/// (with a membership revision) and 89, and 90 holding item 2. Each ordinary
/// playlist has its seed sentinel and live revision; last synced is 8.
const PLAYLISTS: &str = "INSERT INTO container(container_pid,name,is_hidden)VALUES(-3,'Library',1);
    INSERT INTO container(container_pid,name,distinguished_kind,is_hidden)VALUES(77,'Playback History',10000,1);
    INSERT INTO container(container_pid,sync_id,name,is_src_remote,contained_media_type,keep_local_status)VALUES
        (88,88,'First',1,0,2),(89,89,'Second',1,0,2),(90,90,'Held',1,8,2);
    INSERT INTO container_item(container_item_pid,container_pid,item_pid,position,occurrence_id)VALUES(500,-3,2,0,''),(631,90,2,0,'2_0');
    INSERT INTO container_item_media_type(container_pid,media_type,count)VALUES(90,8,1);
    INSERT INTO container_seed(container_pid,item_pid,seed_order)VALUES(88,0,0),(89,0,0),(90,0,0);
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES
        (5,88,0,1,0),(6,88,0,1,1),(7,89,0,1,0),(8,90,0,1,0);
    UPDATE _MLDatabaseProperties SET value='8' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');";
/// Deleting 88 then 89: rows and seeds go, tombstones 9 and 10 in operation
/// order, 88's membership revision goes.
const DELETION: &str = "DELETE FROM container WHERE container_pid IN(88,89);
    DELETE FROM container_seed WHERE container_pid IN(88,89);
    DELETE FROM entity_revision WHERE revision=6;
    UPDATE entity_revision SET revision=9,deleted=2 WHERE revision=5;
    UPDATE entity_revision SET revision=10,deleted=2 WHERE revision=7;
    UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor';
    UPDATE _MLDatabaseProperties SET value='10' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
    UPDATE _MLDatabaseProperties SET value='6' WHERE key='MLSyncClientGenerationID';";

struct Pair(Fixture);
impl Pair {
    fn new(both: &str, after: &str) -> Self {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
        // The fixture's own after state removes a track; start from before.
        fs::copy(&fixture.before, &fixture.after).unwrap();
        for path in [&fixture.before, &fixture.after] {
            Connection::open(path).unwrap().execute_batch(both).unwrap();
        }
        Connection::open(&fixture.after)
            .unwrap()
            .execute_batch(after)
            .unwrap();
        Self(fixture)
    }
    fn tables(&self) -> (Tables, Tables) {
        let dir = &self.0.dir;
        (
            tables(&self.0.before, &dir.join("b")),
            tables(&self.0.after, &dir.join("a")),
        )
    }
}

#[test]
fn empty_playlists_go_with_one_tombstone_each_in_order() {
    let pair = Pair::new(PLAYLISTS, DELETION);
    let (before, after) = pair.tables();
    qualify_deletion(&before, &[88, 89]).unwrap();
    let receipt = verify_deletion(&before, &after, &[88, 89], 10).unwrap();
    assert_eq!(receipt.deleted(), 2);
    assert_eq!(
        receipt.playlists,
        [
            DeletedPlaylist {
                playlist_pid: 88,
                removed_seed_rows: 1,
                revision: (5, 9),
                removed_membership_revision: Some(6),
            },
            DeletedPlaylist {
                playlist_pid: 89,
                removed_seed_rows: 1,
                revision: (7, 10),
                removed_membership_revision: None,
            },
        ]
    );
    assert!(receipt.recent_counter_resets.is_empty());
    assert!(!deletion_not_applied(&before, &after, &[88, 89]).unwrap());
    // Hero progress may stay, recent counters may reset, the maintenance
    // clock may advance; a playlist without a seed row qualifies too.
    let pair = Pair::new(
        &format!(
            "{PLAYLISTS} DELETE FROM container_seed WHERE container_pid=89;
            INSERT INTO _MLDatabaseProperties(key,value)VALUES('MLMaintenanceTaskOperationDate','100.5');"
        ),
        &format!(
            "{DELETION} UPDATE _MLDatabaseProperties SET value='8' WHERE key='MLArtistHeroImageImportDatabaseRevision';
            UPDATE item_stats SET play_count_recent=0 WHERE item_pid=2;
            UPDATE _MLDatabaseProperties SET value='200' WHERE key='MLMaintenanceTaskOperationDate';"
        ),
    );
    let (before, after) = pair.tables();
    let receipt = verify_deletion(&before, &after, &[88, 89], 10).unwrap();
    assert_eq!(receipt.playlists[1].removed_seed_rows, 0);
    assert_eq!(receipt.recent_counter_resets, [2]);
    assert!(
        receipt
            .property_changes
            .contains_key("MLMaintenanceTaskOperationDate")
    );
}

#[test]
fn deletion_refuses_partial_or_extra_changes() {
    let cases = [
        // One playlist stays.
        DELETION.replace("IN(88,89)", "IN(88)").replace(
            "UPDATE entity_revision SET revision=10,deleted=2 WHERE revision=7;",
            "",
        ),
        // A seed row stays.
        DELETION.replace(
            "DELETE FROM container_seed WHERE container_pid IN(88,89);",
            "DELETE FROM container_seed WHERE container_pid=88;",
        ),
        // The membership revision stays.
        DELETION.replace("DELETE FROM entity_revision WHERE revision=6;", ""),
        // The tombstones are out of operation order.
        DELETION
            .replace(
                "revision=9,deleted=2 WHERE revision=5",
                "revision=12,deleted=2 WHERE revision=5",
            )
            .replace("value='10' WHERE key IN", "value='12' WHERE key IN"),
        // A tombstone stays live.
        DELETION.replace("revision=10,deleted=2 WHERE", "revision=10 WHERE"),
        // Last synced stops below the highest tombstone.
        format!(
            "{DELETION} UPDATE _MLDatabaseProperties SET value='9' WHERE key='MLSyncClientLastSyncedRevision';"
        ),
        // Two anchors.
        format!(
            "{DELETION} UPDATE _MLDatabaseProperties SET value='12' WHERE key='MLSyncClientSyncAnchor';"
        ),
        // Hero progress goes elsewhere.
        format!(
            "{DELETION} UPDATE _MLDatabaseProperties SET value='9' WHERE key='MLArtistHeroImageImportDatabaseRevision';"
        ),
        // Another playlist changes.
        format!("{DELETION} UPDATE container SET name='Changed' WHERE container_pid=90;"),
        // A track changes.
        format!("{DELETION} UPDATE item_extra SET title='Changed' WHERE item_pid=2;"),
        // A revision is written for something else.
        format!("{DELETION} UPDATE entity_revision SET revision=11 WHERE revision=8;"),
    ];
    for after in cases {
        let pair = Pair::new(PLAYLISTS, &after);
        let (before, after_tables) = pair.tables();
        assert!(
            verify_deletion(&before, &after_tables, &[88, 89], 10).is_err(),
            "{after}"
        );
    }
    // Wrong prepared anchor.
    let pair = Pair::new(PLAYLISTS, DELETION);
    let (before, after) = pair.tables();
    assert!(verify_deletion(&before, &after, &[88, 89], 9).is_err());
}

#[test]
fn only_empty_unreferenced_ordinary_playlists_qualify() {
    let pair = Pair::new(PLAYLISTS, "");
    let (before, _) = pair.tables();
    for pids in [
        &[][..],
        &[0][..],
        &[88, 88][..],
        // A playlist with a member, the library container, Playback History.
        &[90][..],
        &[-3][..],
        &[77][..],
        &[404][..],
    ] {
        assert!(qualify_deletion(&before, pids).is_err(), "{pids:?}");
    }
    for change in [
        // Smart, hidden, unsynced or cloud playlists.
        "UPDATE container SET smart_is_dynamic=1 WHERE container_pid=88;",
        "UPDATE container SET is_hidden=1 WHERE container_pid=88;",
        "UPDATE container SET is_src_remote=0 WHERE container_pid=88;",
        "UPDATE container SET cloud_global_id='pl.x' WHERE container_pid=88;",
        // A media type although nothing is counted, or a count row (the
        // orphan a populated delete leaves).
        "UPDATE container SET contained_media_type=8 WHERE container_pid=88;",
        "INSERT INTO container_item_media_type(container_pid,media_type,count)VALUES(88,8,0);",
        // A member.
        "INSERT INTO container_item(container_item_pid,container_pid,item_pid,position,occurrence_id)VALUES(601,88,2,0,'2_0');",
        // A child playlist.
        "UPDATE container SET parent_pid=88 WHERE container_pid=90;",
        // A seed that names a track.
        "UPDATE container_seed SET item_pid=2 WHERE container_pid=88;",
        // Artwork of the playlist.
        "INSERT INTO artwork_token(entity_pid,artwork_token,entity_type)VALUES(88,'t',1);",
        // No live revision.
        "DELETE FROM entity_revision WHERE revision=5;",
        // A tombstone among its revisions.
        "UPDATE entity_revision SET deleted=2 WHERE revision=6;",
    ] {
        let pair = Pair::new(&format!("{PLAYLISTS} {change}"), "");
        let (before, _) = pair.tables();
        assert!(qualify_deletion(&before, &[88]).is_err(), "{change}");
    }
}

#[test]
fn an_unapplied_deletion_is_recognised_only_when_nothing_moved() {
    let pair = Pair::new(PLAYLISTS, "");
    let (before, after) = pair.tables();
    assert!(deletion_not_applied(&before, &after, &[88, 89]).unwrap());
    let pair = Pair::new(
        PLAYLISTS,
        "UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor';",
    );
    let (before, after) = pair.tables();
    assert!(!deletion_not_applied(&before, &after, &[88]).unwrap());
    // The anchor stayed but a playlist is gone.
    let pair = Pair::new(
        PLAYLISTS,
        "DELETE FROM container WHERE container_pid=89; UPDATE entity_revision SET revision=9,deleted=2 WHERE revision=7;",
    );
    let (before, after) = pair.tables();
    assert!(deletion_not_applied(&before, &after, &[88]).unwrap());
    assert!(!deletion_not_applied(&before, &after, &[88, 89]).unwrap());
}

#[test]
fn a_later_run_hands_the_rest_to_the_callers_rule_above_the_tombstones() {
    let pair = Pair::new(
        PLAYLISTS,
        &format!("{DELETION} UPDATE item_stats SET play_count_user=100 WHERE item_pid=2;"),
    );
    let (before, after) = pair.tables();
    assert!(verify_deletion(&before, &after, &[88, 89], 10).is_err());
    let floors = std::cell::RefCell::new(Vec::new());
    let rule = |expected: &mut Tables, observed: &Tables, floor: i64| {
        floors.borrow_mut().push(floor);
        expected.insert("item_stats".into(), observed["item_stats"].clone());
        Ok(Some(serde_json::json!({"plays": 1})))
    };
    let receipt = verify_deletion_later(&before, &after, &[88, 89], 10, &rule).unwrap();
    assert_eq!(*floors.borrow(), [10]);
    assert_eq!(receipt.deleted(), 2);
    assert_eq!(
        receipt.between_runs_playback,
        Some(serde_json::json!({"plays": 1}))
    );
}

#[test]
fn a_deletion_session_may_fill_in_the_state_of_an_entry_the_phone_added() {
    use crate::catalogue::removal::tests::{FILLED, PHONE_ENTRY};
    let pair = Pair::new(
        &format!("{PLAYLISTS} {PHONE_ENTRY}"),
        &format!("{DELETION} {FILLED}"),
    );
    let (before, after) = pair.tables();
    let receipt = verify_deletion(&before, &after, &[88, 89], 10).unwrap();
    assert_eq!(receipt.filled_phone_entries, [5]);
}
