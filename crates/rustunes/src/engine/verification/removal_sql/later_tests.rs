//! A removal batch observed by a later run (`resume`, or a rerun of the
//! command), with the between-runs rule `resume` passes it.
use super::*;
use crate::{
    engine::recovery::retained::playback::between_runs,
    engine::verification::sql_tables::{
        Capture,
        reader::{self, Budget},
        tests::Fixture,
    },
};
use music_protocol::catalogue::removal::RemovalReceipt;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn tables(path: &Path, out: &Path) -> Tables {
    reader::materialize(
        Capture {
            database: path,
            database_sha256: Sha256::digest(fs::read(path).unwrap()).into(),
            wal: None,
        },
        out,
        &mut Budget::new(),
    )
    .unwrap()
    .tables
}

/// The native fixture: `both` runs on before and after, then `after` on the
/// after state. Its own after state removes item 1 (tombstone 9, anchor 11);
/// `fresh` starts the after state from before instead.
fn pair(both: &str, after: &str, fresh: bool) -> (Fixture, Tables, Tables) {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    if fresh {
        fs::copy(&fixture.before, &fixture.after).unwrap();
    }
    for path in [&fixture.before, &fixture.after] {
        Connection::open(path).unwrap().execute_batch(both).unwrap();
    }
    Connection::open(&fixture.after)
        .unwrap()
        .execute_batch(after)
        .unwrap();
    let before = tables(&fixture.before, &fixture.dir.join("b"));
    let after = tables(&fixture.after, &fixture.dir.join("a"));
    (fixture, before, after)
}

const CLOCK: &str = "INSERT INTO _MLDatabaseProperties(key,value)VALUES('MLMaintenanceTaskOperationDate','750196509.88088095');";

/// After the removal: survivor 2, whose recent counter the session cleared,
/// played twice and survivor 3 was skipped once; the phone renumbered every
/// survivor above the tombstone, with statistics revisions for both, and its
/// maintenance clock moved.
const LISTENING: &str = "
    UPDATE item_stats SET play_count_user=101,play_count_recent=2,has_been_played=1,date_played=750282000.5 WHERE item_pid=2;
    UPDATE item_stats SET skip_count_user=4,skip_count_recent=1,date_skipped=750282100.5 WHERE item_pid=3;
    UPDATE entity_revision SET revision=revision+8 WHERE class=0 AND revision_type=0 AND deleted=0;
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(13,2,0,0,1),(14,3,0,0,1);
    UPDATE _MLDatabaseProperties SET value='750282909.209113' WHERE key='MLMaintenanceTaskOperationDate';";

fn later(before: &Tables, after: &Tables) -> Result<RemovalReceipt> {
    let rule = |expected: &mut Tables, observed: &Tables, floor: i64| {
        between_runs::absorb_above(expected, observed, &Budget::new(), floor)
    };
    verify_later(before, after, &[1], 10, &rule)
}

#[test]
fn a_later_run_absorbs_listening_after_the_removal() {
    let (_f, before, after) = pair(CLOCK, LISTENING, false);
    assert!(verify(&before, &after, &[1], 10).is_err());
    let receipt = serde_json::to_value(later(&before, &after).unwrap()).unwrap();
    let playback = &receipt["between_runs_playback"];
    let events: Vec<_> = playback["completed_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["item_pid"].as_i64().unwrap(),
                e["play_count_delta"].as_u64().unwrap(),
                e["skip_count_delta"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(events, vec![(2, 2, 0), (3, 0, 1)]);
    // Without listening a later run proves what the session's own run does.
    let (_f, before, after) = pair(CLOCK, "", false);
    let same = serde_json::to_value(verify(&before, &after, &[1], 10).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(later(&before, &after).unwrap()).unwrap(),
        same
    );
}

#[test]
fn a_later_run_absorbs_renumbering_after_the_sessions_own_refresh() {
    // The session refreshed the survivors to 10..12 above tombstone 9; the
    // phone then renumbered them again and dropped item 2's superseded
    // statistics revision, which the same-run contiguity rule cannot explain.
    let both = "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,2,0,0,1);";
    let refresh = "UPDATE entity_revision SET revision=entity_pid+8 WHERE deleted=0 AND class=0 AND revision_type=0;";
    let (_f, before, after) = pair(both, refresh, false);
    verify(&before, &after, &[1], 10).unwrap();
    let housekeeping = "UPDATE entity_revision SET revision=revision+4 WHERE deleted=0 AND class=0 AND revision_type=0;
        DELETE FROM entity_revision WHERE entity_pid=2 AND revision_type=1;";
    let (_f, before, after) = pair(both, &format!("{refresh}{housekeeping}"), false);
    assert!(verify(&before, &after, &[1], 10).is_err());
    later(&before, &after).unwrap();
}

#[test]
fn a_later_run_refuses_what_listening_does_not_explain() {
    for sql in [
        "UPDATE item_extra SET title='Renamed' WHERE item_pid=2",
        "UPDATE item_stats SET user_rating=80 WHERE item_pid=4",
        "UPDATE item_stats SET play_count_user=98 WHERE item_pid=2",
        "UPDATE album SET representative_item_pid=2 WHERE album_pid=11",
        "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(15,77,0,3,0)",
        // Below the tombstones: not the phone's renumbering.
        "UPDATE entity_revision SET revision=8 WHERE entity_pid=4 AND revision_type=0",
        "UPDATE entity_revision SET revision=20 WHERE entity_pid=1",
        "UPDATE _MLDatabaseProperties SET value='12' WHERE key='MLSyncClientSyncAnchor'",
    ] {
        let (_f, before, after) = pair(CLOCK, &format!("{LISTENING}{sql}"), false);
        assert!(later(&before, &after).is_err(), "{sql}");
    }
}

/// Library container -3 and the empty ordinary playlists 88 and 89; last
/// synced is 8.
const PLAYLISTS: &str = "INSERT INTO container(container_pid,name,is_hidden)VALUES(-3,'Library',1);
    INSERT INTO container(container_pid,sync_id,name,is_src_remote,contained_media_type,keep_local_status)VALUES
        (88,88,'First',1,0,2),(89,89,'Second',1,0,2);
    INSERT INTO container_seed(container_pid,item_pid,seed_order)VALUES(88,0,0),(89,0,0);
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,88,0,1,0),(7,89,0,1,0);
    UPDATE _MLDatabaseProperties SET value='8' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');";
/// Deleting 88 then 89 (tombstones 9 and 10), then a play of item 2 and the
/// phone's renumbering above them.
const DELETION_THEN_PLAY: &str = "DELETE FROM container WHERE container_pid IN(88,89);
    DELETE FROM container_seed WHERE container_pid IN(88,89);
    UPDATE entity_revision SET revision=9,deleted=2 WHERE revision=5;
    UPDATE entity_revision SET revision=10,deleted=2 WHERE revision=7;
    UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor';
    UPDATE _MLDatabaseProperties SET value='10' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
    UPDATE _MLDatabaseProperties SET value='6' WHERE key='MLSyncClientGenerationID';
    UPDATE item_stats SET play_count_user=100,play_count_recent=18,has_been_played=1,date_played=750282000.5 WHERE item_pid=2;
    UPDATE entity_revision SET revision=revision+10 WHERE class=0 AND revision_type=0 AND deleted=0;
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(15,2,0,0,1);";

#[test]
fn a_later_run_absorbs_listening_after_a_playlist_deletion() {
    let (_f, before, after) = pair(PLAYLISTS, DELETION_THEN_PLAY, true);
    assert!(verify_deletion(&before, &after, &[88, 89], 10).is_err());
    let rule = |expected: &mut Tables, observed: &Tables, floor: i64| {
        between_runs::absorb_above(expected, observed, &Budget::new(), floor)
    };
    let receipt = verify_deletion_later(&before, &after, &[88, 89], 10, &rule).unwrap();
    assert_eq!(receipt.deleted(), 2);
    let wrong =
        format!("{DELETION_THEN_PLAY}UPDATE item_stats SET user_rating=80 WHERE item_pid=2;");
    let (_f, before, after) = pair(PLAYLISTS, &wrong, true);
    assert!(verify_deletion_later(&before, &after, &[88, 89], 10, &rule).is_err());
}
