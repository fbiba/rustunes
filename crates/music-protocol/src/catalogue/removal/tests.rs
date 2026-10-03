//! Full native schema fixtures for the multi-target removal model.
use super::*;
use crate::catalogue::sqlite::tables::{self as reader, Budget, Capture};
use crate::test_support::catalogue::NativeFixture as Fixture;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

pub(super) fn tables(path: &Path, out: &Path) -> Tables {
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

/// The fixture's after state removes item 1; this also removes item 3 in the
/// same session, with tombstones 9 and 10 and item 3's groups handed to item 4.
const SECOND: &str = "DELETE FROM item WHERE item_pid=3; DELETE FROM item_extra WHERE item_pid=3;
    DELETE FROM item_store WHERE item_pid=3; DELETE FROM item_stats WHERE item_pid=3;
    DELETE FROM item_playback WHERE item_pid=3; DELETE FROM item_search WHERE item_pid=3;
    DELETE FROM item_state WHERE item_pid=3; DELETE FROM item_video WHERE item_pid=3;
    DELETE FROM chapter WHERE item_pid=3; DELETE FROM lyrics WHERE item_pid=3;
    UPDATE entity_revision SET revision=10,deleted=2 WHERE entity_pid=3;
    UPDATE _MLDatabaseProperties SET value='10' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
    UPDATE item_artist SET representative_item_pid=4 WHERE item_artist_pid=13;";

/// Library container -3 (db_info), Playback History 77 and a playlist 88
/// without a sync identity, which is not an ordinary synced playlist.
const CONTAINERS: &str = "INSERT INTO container(container_pid,name,is_hidden)VALUES(-3,'Library',1);
    INSERT INTO container(container_pid,name,distinguished_kind,is_hidden)VALUES(77,'Playback History',10000,1);
    INSERT INTO container(container_pid,name)VALUES(88,'Playlist');
    INSERT INTO container_item(container_item_pid,container_pid,item_pid,position)VALUES(500,-3,1,0),(501,77,1,0),(502,77,1,1),(503,-3,2,1);";

struct Pair {
    fixture: Fixture,
}
impl Pair {
    fn new(both: &str, after: &str) -> Self {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
        for path in [&fixture.before, &fixture.after] {
            Connection::open(path).unwrap().execute_batch(both).unwrap();
        }
        Connection::open(&fixture.after)
            .unwrap()
            .execute_batch(after)
            .unwrap();
        Self { fixture }
    }
    fn tables(&self) -> (Tables, Tables) {
        let dir = &self.fixture.dir;
        let before = tables(&self.fixture.before, &dir.join("b"));
        let after = tables(&self.fixture.after, &dir.join("a"));
        (before, after)
    }
}

#[test]
fn one_session_removes_two_targets_with_retained_memberships() {
    let pair = Pair::new(CONTAINERS, SECOND);
    let (before, after) = pair.tables();
    qualify(&before, &[1, 3]).unwrap();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert_eq!(receipt.removed(), 2);
    assert_eq!(receipt.targets[0].tombstone_revision_after, 9);
    assert_eq!(receipt.targets[1].tombstone_revision_after, 10);
    assert_eq!(receipt.targets[0].retained_memberships, 3);
    assert_eq!(receipt.targets[1].retained_memberships, 0);
    assert!(
        receipt
            .representatives
            .iter()
            .any(|r| r.table == "item_artist"
                && r.group_pid == 13
                && r.before == 3
                && r.after == 4)
    );
    // Order of targets does not matter for the model; both are distinct.
    verify(&before, &after, &[3, 1], 10).unwrap();
}

/// Item 4 has a cover and item 3, artist 13's representative, has none.
const COVERED_FOUR: &str = "INSERT INTO artwork_token(entity_pid,artwork_token,artwork_source_type,artwork_type,entity_type)VALUES(4,'44',300,1,0);";

#[test]
fn an_artist_represented_by_a_track_without_a_cover_moves_to_one_with_a_cover() {
    let pair = Pair::new(
        COVERED_FOUR,
        "UPDATE item_artist SET representative_item_pid=4 WHERE item_artist_pid=13;",
    );
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1], 10).unwrap();
    assert!(
        receipt
            .representatives
            .iter()
            .any(|r| r.table == "item_artist"
                && r.group_pid == 13
                && r.before == 3
                && r.after == 4)
    );
    // Not when the representative has a cover too, nor to a member without one.
    let both = format!(
        "{COVERED_FOUR} INSERT INTO artwork_token(entity_pid,artwork_token,artwork_source_type,artwork_type,entity_type)VALUES(3,'33',300,1,0);"
    );
    let pair = Pair::new(
        &both,
        "UPDATE item_artist SET representative_item_pid=4 WHERE item_artist_pid=13;",
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1], 10).is_err());
    let pair = Pair::new(
        "",
        "UPDATE item_artist SET representative_item_pid=4 WHERE item_artist_pid=13;",
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1], 10).is_err());
}

/// Genre 30 holds every item; item 3, without a cover, represents it.
const GENRE_OF_ALL: &str =
    "INSERT INTO genre(genre_id,genre,representative_item_pid)VALUES(30,'Genre',3);
    UPDATE item SET genre_id=30;";

#[test]
fn a_genre_represented_by_a_track_without_a_cover_moves_to_one_with_a_cover() {
    // Seen on the phone: a genre's representative moved off a surviving
    // coverless track to the lowest surviving covered one.
    let pair = Pair::new(
        &format!("{GENRE_OF_ALL} {COVERED_FOUR}"),
        "UPDATE genre SET representative_item_pid=4 WHERE genre_id=30;",
    );
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1], 10).unwrap();
    assert!(
        receipt
            .representatives
            .iter()
            .any(|r| r.table == "genre" && r.group_pid == 30 && r.before == 3 && r.after == 4)
    );
    // Not to a member without a cover.
    let pair = Pair::new(
        GENRE_OF_ALL,
        "UPDATE genre SET representative_item_pid=4 WHERE genre_id=30;",
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1], 10).is_err());
}

/// An entry the phone added itself (a streamed track): item 5, with no sync
/// ID, no file and its state at the schema defaults.
pub(super) const PHONE_ENTRY: &str = "INSERT INTO item(item_pid,media_type)VALUES(5,8);
    INSERT INTO item_store(item_pid,sync_id)VALUES(5,0);";
/// That state as the first session after the phone added the entry left it.
pub(super) const FILLED: &str = "UPDATE item_state SET contained_persistent_id=NULL,lyrics_adam_id=NULL,is_valid_content_type=1 WHERE item_pid=5;";

#[test]
fn a_session_fills_in_the_state_of_an_entry_the_phone_added() {
    let pair = Pair::new(PHONE_ENTRY, FILLED);
    let (before, after) = pair.tables();
    assert_eq!(
        verify(&before, &after, &[1], 10)
            .unwrap()
            .filled_phone_entries,
        [5]
    );
    // Left alone, it stays.
    let pair = Pair::new(PHONE_ENTRY, "");
    let (before, after) = pair.tables();
    assert!(
        verify(&before, &after, &[1], 10)
            .unwrap()
            .filled_phone_entries
            .is_empty()
    );
    // Not in part, not with any other cell, and not for a track a sync made.
    for after in [
        "UPDATE item_state SET is_valid_content_type=1 WHERE item_pid=5;".to_owned(),
        format!("{FILLED} UPDATE item_state SET title='Elsewhere' WHERE item_pid=5;"),
        FILLED.replace("item_pid=5", "item_pid=2"),
    ] {
        let pair = Pair::new(PHONE_ENTRY, &after);
        let (before, after) = pair.tables();
        assert!(verify(&before, &after, &[1], 10).is_err(), "{after:?}");
    }
}

#[test]
fn removal_refuses_partial_or_extra_changes() {
    let cases = [
        // Only item 1 removed although both were sent.
        ("", String::new()),
        // A membership row removed although the receiver keeps them.
        (
            CONTAINERS,
            format!("{SECOND} DELETE FROM container_item WHERE container_item_pid=501;"),
        ),
        // A surviving group's representative moved to an unrelated item.
        (
            "",
            format!(
                "{SECOND} UPDATE item_artist SET representative_item_pid=2 WHERE item_artist_pid=13;"
            ),
        ),
        // A survivor's row changed.
        (
            "",
            format!("{SECOND} UPDATE item_extra SET title='Changed' WHERE item_pid=2;"),
        ),
        // Last synced does not follow the highest tombstone.
        (
            "",
            format!(
                "{SECOND} UPDATE _MLDatabaseProperties SET value='9' WHERE key='MLSyncClientLastSyncedRevision';"
            ),
        ),
    ];
    for (both, after) in cases {
        let pair = Pair::new(both, &after);
        let (before, after_tables) = pair.tables();
        assert!(
            verify(&before, &after_tables, &[1, 3], 10).is_err(),
            "{after}"
        );
    }
}

/// Album 11 with a live album revision, emptied by removing items 1, 3 and 4.
const ALBUM_REVISION: &str = "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,11,0,4,0);";
const EMPTIED: &str = "DELETE FROM item WHERE item_pid IN(3,4); DELETE FROM item_extra WHERE item_pid IN(3,4);
    DELETE FROM item_store WHERE item_pid IN(3,4); DELETE FROM item_stats WHERE item_pid IN(3,4);
    DELETE FROM item_playback WHERE item_pid IN(3,4); DELETE FROM item_search WHERE item_pid IN(3,4);
    DELETE FROM item_state WHERE item_pid IN(3,4); DELETE FROM item_video WHERE item_pid IN(3,4);
    DELETE FROM chapter WHERE item_pid IN(3,4); DELETE FROM lyrics WHERE item_pid IN(3,4);
    UPDATE entity_revision SET revision=10,deleted=2 WHERE entity_pid=3 AND class=0;
    UPDATE entity_revision SET revision=11,deleted=2 WHERE entity_pid=4 AND class=0;
    UPDATE entity_revision SET revision=12 WHERE entity_pid=11 AND class=4;
    UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientLastSyncedRevision';
    UPDATE album SET representative_item_pid=0,keep_local_status=2 WHERE album_pid=11;
    UPDATE item_artist SET representative_item_pid=0 WHERE item_artist_pid=13;";

/// Seen on the phone: an emptied album's revision moved above the tombstones
/// and hero progress followed that album revision.
#[test]
fn hero_progress_may_follow_an_emptied_album_revision() {
    for (hero, accepted) in [
        ("12", true),
        ("11", true),
        ("4", true),
        ("13", false),
        ("10", false),
    ] {
        let pair = Pair::new(
            ALBUM_REVISION,
            &format!(
                "{EMPTIED} UPDATE _MLDatabaseProperties SET value='{hero}' WHERE key='MLArtistHeroImageImportDatabaseRevision';"
            ),
        );
        let (before, after) = pair.tables();
        let result = verify(&before, &after, &[1, 3, 4], 10);
        assert_eq!(result.is_ok(), accepted, "hero {hero}: {result:?}");
        if let Ok(receipt) = result {
            assert_eq!(receipt.emptied_albums.len(), 1);
            assert_eq!(receipt.emptied_albums[0].revision, Some((5, 12)));
        }
    }
}

/// Album 10 keeps item 2 when items 1 and 3 go. An earlier session left it
/// at keep_local_status 0, with a live revision 5 and a stats revision 6.
const UNEVALUATED: &str = "UPDATE album SET keep_local_status=2; UPDATE album SET keep_local_status=0 WHERE album_pid=10;
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,10,0,4,0),(6,10,0,4,1);";
/// The session evaluates it as a metadata edit did: status 2, its revision
/// moved directly above the tombstones (9 and 10), its stats revision gone.
const EVALUATED: &str = "UPDATE album SET keep_local_status=2 WHERE album_pid=10;
    UPDATE entity_revision SET revision=11 WHERE entity_pid=10 AND class=4 AND revision_type=0;
    DELETE FROM entity_revision WHERE entity_pid=10 AND class=4 AND revision_type=1;";

/// An insert batch killed mid-upload left its album unevaluated with a member
/// kept; the next session may evaluate it.
#[test]
fn an_album_left_unevaluated_may_be_evaluated_though_it_keeps_members() {
    let pair = Pair::new(UNEVALUATED, &format!("{SECOND} {EVALUATED}"));
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert!(receipt.emptied_albums.is_empty());
    assert_eq!(
        receipt.evaluated_albums,
        [EmptiedAlbum {
            album_pid: 10,
            keep_local_status: Some((0, 2)),
            revision: Some((5, 11)),
            removed_stats_revision: Some(6),
        }]
    );
    // Without it the session verifies as before, and its receipt says nothing.
    let pair = Pair::new(UNEVALUATED, SECOND);
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert!(receipt.evaluated_albums.is_empty());
    assert!(
        serde_json::to_value(&receipt)
            .unwrap()
            .get("evaluated_albums")
            .is_none()
    );
    for (both, change, why) in [
        (
            UNEVALUATED,
            format!("{EVALUATED} UPDATE album SET sort_album='Changed' WHERE album_pid=10;"),
            "another cell changed",
        ),
        (
            UNEVALUATED,
            "UPDATE album SET keep_local_status=2 WHERE album_pid=10;".to_owned(),
            "the revision stayed",
        ),
        (
            UNEVALUATED,
            EVALUATED.replace("revision=11", "revision=12"),
            "the revision left a gap above the tombstones",
        ),
        (
            UNEVALUATED,
            EVALUATED.replace("keep_local_status=2", "keep_local_status=1"),
            "another status",
        ),
        (
            &UNEVALUATED.replace("keep_local_status=0 WHERE", "keep_local_status=1 WHERE"),
            EVALUATED.to_owned(),
            "the album was evaluated already",
        ),
    ] {
        let pair = Pair::new(both, &format!("{SECOND} {change}"));
        let (before, after) = pair.tables();
        assert!(verify(&before, &after, &[1, 3], 10).is_err(), "{why}");
    }
}

#[test]
fn qualification_refuses_other_containers_duplicates_and_missing_targets() {
    let pair = Pair::new(
        &format!(
            "{CONTAINERS} INSERT INTO container_item(container_item_pid,container_pid,item_pid)VALUES(504,88,3);"
        ),
        "",
    );
    let (before, _) = pair.tables();
    qualify(&before, &[1]).unwrap();
    assert!(qualify(&before, &[3]).is_err());
    assert!(qualify(&before, &[1, 1]).is_err());
    assert!(qualify(&before, &[]).is_err());
    assert!(qualify(&before, &[99]).is_err());
}

#[test]
fn unapplied_session_is_recognised_only_when_nothing_moved() {
    let pair = Pair::new(CONTAINERS, SECOND);
    let (before, after) = pair.tables();
    assert!(not_applied(&before, &before, &[1, 3]).unwrap());
    assert!(!not_applied(&before, &after, &[1, 3]).unwrap());
}

/// Album 11 (and its album artist 21) and item artists 11 and 13 lose every
/// member when items 1, 3 and 4 go; each group has a live revision, item
/// artist 13 a stats revision too, and album 11 a cover token.
const GROUP_REVISIONS: &str = "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,11,0,4,0),(6,21,0,7,0),(7,13,0,2,0),(8,13,0,2,1),(16,11,0,2,0);
    INSERT INTO artwork_token(entity_pid,artwork_token,artwork_source_type,artwork_type,entity_type)VALUES(11,'900',300,6,4);";
/// Seen in a 100-target removal: the session also deleted every
/// group no item belonged to, tombstoned each group's live revision above the
/// item tombstones, dropped its stats revision and kept its artwork tokens.
const COLLECTED: &str = "DELETE FROM item WHERE item_pid IN(3,4); DELETE FROM item_extra WHERE item_pid IN(3,4);
    DELETE FROM item_store WHERE item_pid IN(3,4); DELETE FROM item_stats WHERE item_pid IN(3,4);
    DELETE FROM item_playback WHERE item_pid IN(3,4); DELETE FROM item_search WHERE item_pid IN(3,4);
    DELETE FROM item_state WHERE item_pid IN(3,4); DELETE FROM item_video WHERE item_pid IN(3,4);
    DELETE FROM chapter WHERE item_pid IN(3,4); DELETE FROM lyrics WHERE item_pid IN(3,4);
    UPDATE entity_revision SET revision=10,deleted=2 WHERE entity_pid=3 AND class=0;
    UPDATE entity_revision SET revision=11,deleted=2 WHERE entity_pid=4 AND class=0;
    DELETE FROM album WHERE album_pid=11; DELETE FROM album_artist WHERE album_artist_pid=21;
    DELETE FROM item_artist WHERE item_artist_pid IN(11,13);
    UPDATE entity_revision SET revision=12,deleted=2 WHERE entity_pid=11 AND class=4;
    UPDATE entity_revision SET revision=13,deleted=2 WHERE entity_pid=21 AND class=7;
    DELETE FROM entity_revision WHERE entity_pid=13 AND class=2 AND revision_type=1;
    UPDATE entity_revision SET revision=14,deleted=2 WHERE entity_pid=13 AND class=2 AND revision_type=0;
    UPDATE entity_revision SET revision=17,deleted=2 WHERE entity_pid=11 AND class=2;
    UPDATE _MLDatabaseProperties SET value='17' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');";

#[test]
fn a_session_may_collect_every_group_without_members() {
    let pair = Pair::new(GROUP_REVISIONS, COLLECTED);
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3, 4], 10).unwrap();
    let collected: Vec<_> = receipt
        .collected_groups
        .iter()
        .map(|g| (g.table, g.group_pid, g.tombstone, g.removed_stats_revision))
        .collect();
    assert_eq!(
        collected,
        [
            ("album", 11, Some((5, 12)), None),
            ("album_artist", 21, Some((6, 13)), None),
            ("item_artist", 11, Some((16, 17)), None),
            ("item_artist", 13, Some((7, 14)), Some(8)),
        ]
    );
    // Survivors' groups stay; nothing else may go with them.
    let cases = [
        // A group that keeps a member is deleted too.
        format!("{COLLECTED} DELETE FROM album WHERE album_pid=10;"),
        // A collected group's tombstone is not above the last synced revision.
        COLLECTED.replace("revision=12,deleted=2", "revision=3,deleted=2"),
        // Its stats revision stays.
        COLLECTED.replace(
            "DELETE FROM entity_revision WHERE entity_pid=13 AND class=2 AND revision_type=1;",
            "",
        ),
        // Its cover token goes.
        format!("{COLLECTED} DELETE FROM artwork_token WHERE entity_pid=11 AND entity_type=4;"),
        // Last synced stops at the item tombstones.
        format!(
            "{COLLECTED} UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientLastSyncedRevision';"
        ),
    ];
    for after in cases {
        let pair = Pair::new(GROUP_REVISIONS, &after);
        let (before, after_tables) = pair.tables();
        assert!(
            verify(&before, &after_tables, &[1, 3, 4], 10).is_err(),
            "{after}"
        );
    }
}

/// Cover tokens of album 99 and item artist 98, which no longer exist, and of
/// album 10, which does.
const ORPHAN_TOKENS: &str = "INSERT INTO artwork_token(entity_pid,artwork_token,artwork_source_type,artwork_type,entity_type)VALUES(99,'901',300,6,4),(98,'x',500,2,2),(10,'902',300,6,4);
    INSERT INTO best_artwork_token(entity_pid,entity_type,artwork_type,available_artwork_token)VALUES(99,4,6,'901'),(10,4,6,'902');";

/// Seen on the phone: the session after one that collected groups deleted
/// every token row whose entity that earlier session had collected, and only
/// those.
#[test]
fn a_session_may_drop_tokens_whose_entity_was_already_gone() {
    let pair = Pair::new(
        ORPHAN_TOKENS,
        "DELETE FROM artwork_token WHERE entity_pid IN(98,99); DELETE FROM best_artwork_token WHERE entity_pid=99;",
    );
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1], 10).unwrap();
    assert_eq!(receipt.removed_orphan_artwork_rows, 3);
    // Keeping them is the earlier shape.
    let pair = Pair::new(ORPHAN_TOKENS, "");
    let (before, after) = pair.tables();
    assert_eq!(
        verify(&before, &after, &[1], 10)
            .unwrap()
            .removed_orphan_artwork_rows,
        0
    );
    // A token of a live album never goes.
    let pair = Pair::new(
        ORPHAN_TOKENS,
        "DELETE FROM artwork_token WHERE entity_pid=10;",
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1], 10).is_err());
}

/// Ordinary synced playlists: 88 [1,2,4], 89 [1], 90 [2,3,4,1] and 91 [2,4],
/// each with its count row, seed sentinel, live revision and (but 91) a
/// membership revision. Revision 9 is left free for the base tombstone.
const PLAYLISTS: &str = "INSERT INTO container(container_pid,sync_id,name,is_src_remote,contained_media_type,keep_local_status)VALUES
        (88,88,'Head',1,8,2),(89,89,'Sole',1,8,2),(90,90,'Middle and tail',1,8,2),(91,91,'Untouched',1,8,2);
    INSERT INTO container_item(container_item_pid,container_pid,item_pid,position,occurrence_id)VALUES
        (601,88,1,0,'1_0'),(602,88,2,1,'2_0'),(603,88,4,2,'4_0'),(611,89,1,0,'1_0'),
        (621,90,2,0,'2_0'),(622,90,3,1,'3_0'),(623,90,4,2,'4_0'),(624,90,1,3,'1_0'),
        (631,91,2,0,'2_0'),(632,91,4,1,'4_0');
    INSERT INTO container_item_media_type(container_pid,media_type,count)VALUES(88,8,3),(89,8,1),(90,8,4),(91,8,2);
    INSERT INTO container_seed(container_pid,item_pid,seed_order)VALUES(88,0,0),(89,0,0),(90,0,0),(91,0,0);
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES
        (5,88,0,1,0),(6,88,0,1,1),(7,89,0,1,0),(8,89,0,1,1),(10,90,0,1,0),(11,90,0,1,1),(12,91,0,1,0);";
/// Seen on the phone: removing items 1 and 3 (on top of the base
/// after state, which removed item 1) closes up 88 and 90 from position 0,
/// empties 89, and moves each playlist's live revision among the tombstones
/// while its membership revision goes.
const PLAYLIST_REMOVAL: &str = "DELETE FROM item WHERE item_pid=3; DELETE FROM item_extra WHERE item_pid=3;
    DELETE FROM item_store WHERE item_pid=3; DELETE FROM item_stats WHERE item_pid=3;
    DELETE FROM item_playback WHERE item_pid=3; DELETE FROM item_search WHERE item_pid=3;
    DELETE FROM item_state WHERE item_pid=3; DELETE FROM item_video WHERE item_pid=3;
    DELETE FROM chapter WHERE item_pid=3; DELETE FROM lyrics WHERE item_pid=3;
    UPDATE entity_revision SET revision=13 WHERE entity_pid=1 AND class=0;
    UPDATE entity_revision SET revision=14,deleted=2 WHERE entity_pid=3 AND class=0;
    UPDATE item_artist SET representative_item_pid=4 WHERE item_artist_pid=13;
    DELETE FROM container_item WHERE container_item_pid IN(601,611,622,624);
    UPDATE container_item SET position=0 WHERE container_item_pid=602;
    UPDATE container_item SET position=1 WHERE container_item_pid IN(603,623);
    UPDATE container_item_media_type SET count=2 WHERE container_pid IN(88,90);
    DELETE FROM container_item_media_type WHERE container_pid=89;
    UPDATE container SET contained_media_type=0 WHERE container_pid=89;
    DELETE FROM entity_revision WHERE revision IN(6,8,11);
    UPDATE entity_revision SET revision=15 WHERE revision=5;
    UPDATE entity_revision SET revision=16 WHERE revision=7;
    UPDATE entity_revision SET revision=17 WHERE revision=10;
    UPDATE _MLDatabaseProperties SET value='17' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');";

#[test]
fn playlists_lose_removed_members_and_close_up() {
    let pair = Pair::new(PLAYLISTS, PLAYLIST_REMOVAL);
    let (before, after) = pair.tables();
    qualify(&before, &[1, 3]).unwrap();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    let effects: Vec<_> = receipt
        .playlists
        .iter()
        .map(|p| {
            (
                p.playlist_pid,
                p.removed_members.clone(),
                p.moved_members,
                p.emptied,
                p.revision,
                p.removed_membership_revision,
            )
        })
        .collect();
    assert_eq!(
        effects,
        [
            (88, vec![601], 2, false, (5, 15), Some(6)),
            (89, vec![611], 0, true, (7, 16), Some(8)),
            (90, vec![622, 624], 1, false, (10, 17), Some(11)),
        ]
    );
    assert_eq!(receipt.targets[0].retained_memberships, 0);
}

#[test]
fn playlist_removal_refuses_other_shapes() {
    let cases = [
        // The survivors keep their old positions.
        PLAYLIST_REMOVAL.replace(
            "UPDATE container_item SET position=0 WHERE container_item_pid=602;",
            "",
        ),
        // The count does not follow.
        PLAYLIST_REMOVAL.replace(
            "UPDATE container_item_media_type SET count=2 WHERE container_pid IN(88,90);",
            "",
        ),
        // The emptied playlist keeps its media type.
        PLAYLIST_REMOVAL.replace(
            "UPDATE container SET contained_media_type=0 WHERE container_pid=89;",
            "",
        ),
        // An emptied playlist keeps its count row.
        PLAYLIST_REMOVAL.replace(
            "DELETE FROM container_item_media_type WHERE container_pid=89;",
            "",
        ),
        // A playlist's live revision stays.
        PLAYLIST_REMOVAL.replace(
            "UPDATE entity_revision SET revision=15 WHERE revision=5;",
            "",
        ),
        // A membership revision stays.
        PLAYLIST_REMOVAL.replace("revision IN(6,8,11)", "revision IN(8,11)"),
        // Last synced stops at the item tombstones.
        format!(
            "{PLAYLIST_REMOVAL} UPDATE _MLDatabaseProperties SET value='14' WHERE key='MLSyncClientLastSyncedRevision';"
        ),
        // A playlist without targets changes.
        format!(
            "{PLAYLIST_REMOVAL} UPDATE container_item SET position=5 WHERE container_item_pid=632;"
        ),
        // A target's membership row stays.
        PLAYLIST_REMOVAL.replace("IN(601,611,622,624)", "IN(601,611,622)"),
        // A membership revision appears without a keep-local evaluation.
        format!(
            "{PLAYLIST_REMOVAL} INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,88,0,1,1);"
        ),
    ];
    for after in cases {
        let pair = Pair::new(PLAYLISTS, &after);
        let (before, after_tables) = pair.tables();
        assert!(
            verify(&before, &after_tables, &[1, 3], 10).is_err(),
            "{after}"
        );
    }
}

/// In the same removal, a playlist created minutes earlier (88 here) was
/// still at keep_local_status 0; the session set it to 2 and wrote a new
/// membership revision above everything else it wrote.
#[test]
fn a_playlist_may_be_evaluated_for_keeping_local_in_the_session() {
    let unevaluated =
        format!("{PLAYLISTS} UPDATE container SET keep_local_status=0 WHERE container_pid=88;");
    let evaluated = format!(
        "{PLAYLIST_REMOVAL} UPDATE container SET keep_local_status=2 WHERE container_pid=88;
        INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,88,0,1,1);"
    );
    let pair = Pair::new(&unevaluated, &evaluated);
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert_eq!(receipt.playlists[0].keep_local_revision, Some(18));
    for after in [
        // The new membership revision is not above the session's revisions.
        evaluated.replace("VALUES(18,88,0,1,1)", "VALUES(3,88,0,1,1)"),
        // The status moved without a membership revision.
        evaluated.replace("INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,88,0,1,1);", ""),
        // The status moved elsewhere than 2.
        evaluated.replace("keep_local_status=2 WHERE container_pid=88", "keep_local_status=1 WHERE container_pid=88"),
    ] {
        let pair = Pair::new(&unevaluated, &after);
        let (before, after_tables) = pair.tables();
        assert!(verify(&before, &after_tables, &[1, 3], 10).is_err(), "{after}");
    }
}

/// Seen on the phone: a playlist created minutes earlier and holding no
/// target (91 here) was still at keep_local_status 0; the session set it to
/// 2 and replaced its membership revision with one above everything else it
/// wrote.
#[test]
fn an_untouched_playlist_may_be_evaluated_for_keeping_local_in_the_session() {
    let unevaluated = format!(
        "{PLAYLISTS} UPDATE container SET keep_local_status=0 WHERE container_pid=91;
        INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(30,91,0,1,1);"
    );
    let evaluated = format!(
        "{PLAYLIST_REMOVAL} UPDATE container SET keep_local_status=2 WHERE container_pid=91;
        UPDATE entity_revision SET revision=31 WHERE revision=30;"
    );
    let pair = Pair::new(&unevaluated, &evaluated);
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert_eq!(
        receipt.keep_local_evaluations,
        [playlists::KeepLocalEvaluation {
            playlist_pid: 91,
            removed_membership_revision: Some(30),
            keep_local_revision: Some(31),
        }]
    );
    // Without an earlier membership revision, the evaluation writes one.
    let pair = Pair::new(
        &format!("{PLAYLISTS} UPDATE container SET keep_local_status=0 WHERE container_pid=91;"),
        &format!(
            "{PLAYLIST_REMOVAL} UPDATE container SET keep_local_status=2 WHERE container_pid=91;
            INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,91,0,1,1);"
        ),
    );
    let (before, after) = pair.tables();
    let receipt = verify(&before, &after, &[1, 3], 10).unwrap();
    assert_eq!(
        receipt.keep_local_evaluations[0].keep_local_revision,
        Some(18)
    );
    for after in [
        // The new membership revision is not above the session's revisions.
        evaluated.replace(
            "SET revision=31 WHERE revision=30",
            "SET revision=11 WHERE revision=30",
        ),
        // The status moved without a new membership revision.
        evaluated.replace(
            "UPDATE entity_revision SET revision=31 WHERE revision=30;",
            "",
        ),
        // The membership revision moved without the status.
        evaluated.replace(
            "UPDATE container SET keep_local_status=2 WHERE container_pid=91;",
            "",
        ),
        // The status moved elsewhere than 2.
        evaluated.replace(
            "keep_local_status=2 WHERE container_pid=91",
            "keep_local_status=1 WHERE container_pid=91",
        ),
        // Something else about the playlist changed too.
        format!("{evaluated} UPDATE container SET name='Changed' WHERE container_pid=91;"),
    ] {
        let pair = Pair::new(&unevaluated, &after);
        let (before, after_tables) = pair.tables();
        assert!(
            verify(&before, &after_tables, &[1, 3], 10).is_err(),
            "{after}"
        );
    }
    // A playlist already evaluated gets no new membership revision.
    let pair = Pair::new(
        PLAYLISTS,
        &format!(
            "{PLAYLIST_REMOVAL} INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,91,0,1,1);"
        ),
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1, 3], 10).is_err());
}

#[test]
fn qualification_accepts_only_modelled_playlists() {
    let pair = Pair::new(PLAYLISTS, "");
    let (before, _) = pair.tables();
    qualify(&before, &[1, 3]).unwrap();
    // Item 2 belongs to 91, whose shape is modelled too.
    qualify(&before, &[2]).unwrap();
    for change in [
        // Smart, hidden, a folder's child, unsynced or cloud playlists.
        "UPDATE container SET smart_is_dynamic=1 WHERE container_pid=88;",
        "UPDATE container SET smart_criteria=x'00' WHERE container_pid=88;",
        "UPDATE container SET is_hidden=1 WHERE container_pid=88;",
        "UPDATE container SET parent_pid=7 WHERE container_pid=88;",
        "UPDATE container SET is_src_remote=0 WHERE container_pid=88;",
        "UPDATE container SET cloud_global_id='pl.x' WHERE container_pid=88;",
        // The same target twice in one playlist.
        "INSERT INTO container_item(container_item_pid,container_pid,item_pid,position,occurrence_id)VALUES(604,88,1,3,'1_1');
         UPDATE container_item_media_type SET count=4 WHERE container_pid=88;",
        // A gap in the positions.
        "UPDATE container_item SET position=5 WHERE container_item_pid=603;",
        // A member that is not counted as music.
        "UPDATE container_item_media_type SET count=2 WHERE container_pid=88;",
        // No live revision.
        "DELETE FROM entity_revision WHERE revision=5;",
    ] {
        let pair = Pair::new(&format!("{PLAYLISTS} {change}"), "");
        let (before, _) = pair.tables();
        assert!(qualify(&before, &[1]).is_err(), "{change}");
    }
}

#[test]
fn a_session_that_also_inserts_removes_no_playlist_member() {
    // Library and Playback History memberships stay on the phone.
    let pair = Pair::new(CONTAINERS, "");
    let (before, _) = pair.tables();
    qualify_session(&before, &[1, 3]).unwrap();
    let pair = Pair::new(PLAYLISTS, "");
    let (before, _) = pair.tables();
    assert_eq!(
        playlists::holding(&before, &BTreeSet::from([3, 4])).unwrap(),
        BTreeSet::from([88, 90, 91])
    );
    qualify(&before, &[3]).unwrap();
    assert!(qualify_session(&before, &[3]).is_err());
}

/// A rule standing in for the caller's between-runs rule: it records the
/// floor it was given and takes `tables` from the after state.
fn taking<'a>(
    floors: &'a std::cell::RefCell<Vec<i64>>,
    tables: &'a [&'a str],
) -> impl Fn(&mut Tables, &Tables, i64) -> Result<Option<serde_json::Value>> + 'a {
    move |expected, observed, floor| {
        floors.borrow_mut().push(floor);
        for name in tables {
            expected.insert((*name).into(), observed[*name].clone());
        }
        Ok(Some(serde_json::json!({ "taken": tables })))
    }
}

#[test]
fn a_later_run_leaves_survivor_renumbering_to_the_callers_rule() {
    // The session refreshed the survivors to 10..12 above tombstone 9 and
    // moved hero progress to 12; the phone later renumbered them to 14..16.
    let pair = Pair::new(
        "",
        "UPDATE entity_revision SET revision=entity_pid+12 WHERE deleted=0 AND class=0 AND revision_type=0;
        UPDATE _MLDatabaseProperties SET value='12' WHERE key='MLArtistHeroImageImportDatabaseRevision';",
    );
    let (before, after) = pair.tables();
    assert!(verify(&before, &after, &[1], 10).is_err());
    let floors = std::cell::RefCell::new(Vec::new());
    let rule = taking(&floors, &["entity_revision"]);
    let receipt = verify_later(&before, &after, &[1], 10, &rule).unwrap();
    assert_eq!(*floors.borrow(), [9]);
    assert!(receipt.survivor_revision_refresh.is_empty());
    assert_eq!(
        receipt.between_runs_playback,
        Some(serde_json::json!({"taken": ["entity_revision"]}))
    );
    // The rule decides what it is given.
    let refuse = |_: &mut Tables, _: &Tables, _: i64| -> Result<Option<serde_json::Value>> {
        bail!("not listening")
    };
    assert!(verify_later(&before, &after, &[1], 10, &refuse).is_err());
    // Hero progress may stop above the tombstones, never beyond the state.
    let pair = Pair::new(
        "",
        "UPDATE _MLDatabaseProperties SET value='30' WHERE key='MLArtistHeroImageImportDatabaseRevision';",
    );
    let (before, after) = pair.tables();
    assert!(verify_later(&before, &after, &[1], 10, &rule).is_err());
}

#[test]
fn a_later_run_still_models_what_the_session_wrote() {
    // The rule is not asked about the tables the session itself changed.
    let floors = std::cell::RefCell::new(Vec::new());
    let rule = taking(&floors, &["item_stats"]);
    for sql in [
        "UPDATE entity_revision SET revision=20 WHERE entity_pid=1",
        "UPDATE album SET representative_item_pid=2 WHERE album_pid=11",
        "UPDATE _MLDatabaseProperties SET value='12' WHERE key='MLSyncClientSyncAnchor'",
        "UPDATE item_extra SET title='Renamed' WHERE item_pid=2",
    ] {
        let pair = Pair::new("", sql);
        let (before, after) = pair.tables();
        assert!(
            verify_later(&before, &after, &[1], 10, &rule).is_err(),
            "{sql}"
        );
    }
    let pair = Pair::new("", "");
    let (before, after) = pair.tables();
    let receipt = verify_later(&before, &after, &[1], 10, &rule).unwrap();
    assert_eq!(receipt.removed(), 1);
}

#[test]
fn a_later_run_recognises_an_unapplied_session_whose_target_was_played() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = &fixture.dir;
    let before = tables(&fixture.before, &dir.join("b"));
    let later = |sql: &str, name: &str| {
        fs::copy(&fixture.before, &fixture.after).unwrap();
        Connection::open(&fixture.after)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        tables(&fixture.after, &dir.join(name))
    };
    // Target 1 played once, and the phone renumbered every live item.
    let played = "UPDATE item_stats SET play_count_user=6,play_count_recent=6,has_been_played=1,date_played=750282000.5 WHERE item_pid=1;
        UPDATE entity_revision SET revision=revision+8 WHERE class=0 AND revision_type=0;";
    let after = later(played, "played");
    assert!(!not_applied(&before, &after, &[1]).unwrap());
    assert!(not_applied_later(&before, &after, &[1]).unwrap());
    for (n, sql) in [
        "UPDATE item_extra SET title='Renamed' WHERE item_pid=1",
        "UPDATE item_stats SET play_count_user=4 WHERE item_pid=1",
        "UPDATE item_stats SET user_rating=80 WHERE item_pid=1",
        "UPDATE entity_revision SET deleted=2 WHERE entity_pid=1",
        "UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor'",
    ]
    .into_iter()
    .enumerate()
    {
        let after = later(&format!("{played}{sql}"), &format!("wrong-{n}"));
        assert!(!not_applied_later(&before, &after, &[1]).unwrap(), "{sql}");
    }
}
