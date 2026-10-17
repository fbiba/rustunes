//! Full native-schema fixtures for the metadata edit model.
use super::*;
use crate::{
    engine::planning::metadata_edit::{self as planning, check_conformant},
    engine::verification::sql_tables::{
        Capture,
        reader::{self, Budget},
        tests::Fixture,
    },
};
use music_protocol::catalogue::metadata_edit::{Envelope, TrackUpdate, encode};
use plist::Value;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

// The synthetic library: five tracks, sync ids 1001..1005.
//
//   item  title    artist(IA)    album(AL)     album artist(AA)  genre  composer
//   1     Track 1  Target(11)*   Album(10)     Artist(20)*       Rock   -
//   2     Track 2  Survivor(12)* Album(10)*    Artist(20)        Rock   Comp A(41)*
//   3     Track 3  Other(13)*    Other(11)*    Other(21)*        Pop*   -
//   4     Track 4  Other(13)     Other(11)     Other(21)         Pop    -
//   5     Track 5  Survivor(12)  Third(13)*    Artist(20)        Rock   -
//
// `*` marks the stored representative. Album 12 "Second" (album artist 20)
// is an empty row. Library container -3, database -2, anchor 10, highest
// revision 17.
pub(crate) const NAMES: &[&str] = &[
    "Track 1", "Track 2", "Track 3", "Track 4", "Track 5", "Target", "Survivor", "Other", "Album",
    "Second", "Third", "Artist", "Rock", "Pop", "Comp A",
];
pub(crate) const DATABASE: i64 = -2;
pub(crate) const CONTAINER: i64 = -3;
pub(crate) const ANCHOR_BEFORE: u32 = 10;

fn gk_hex(name: &str) -> String {
    name.to_lowercase()
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn sort_key_hex(name: &str) -> String {
    format!("30{}0107", gk_hex(name))
}
fn base_order(name: &str) -> i64 {
    1000 + 10
        * NAMES
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("{name} is not a base name")) as i64
}

struct Spec {
    pid: i64,
    title: &'static str,
    ia: (i64, &'static str),
    album: (i64, &'static str),
    aa: (i64, &'static str),
    composer: Option<(i64, &'static str)>,
    genre: (i64, &'static str),
    year: i64,
}
const SPECS: [Spec; 5] = [
    Spec {
        pid: 1,
        title: "Track 1",
        ia: (11, "Target"),
        album: (10, "Album"),
        aa: (20, "Artist"),
        composer: None,
        genre: (31, "Rock"),
        year: 2000,
    },
    Spec {
        pid: 2,
        title: "Track 2",
        ia: (12, "Survivor"),
        album: (10, "Album"),
        aa: (20, "Artist"),
        composer: Some((41, "Comp A")),
        genre: (31, "Rock"),
        year: 2001,
    },
    Spec {
        pid: 3,
        title: "Track 3",
        ia: (13, "Other"),
        album: (11, "Other"),
        aa: (21, "Other"),
        composer: None,
        genre: (32, "Pop"),
        year: 2002,
    },
    Spec {
        pid: 4,
        title: "Track 4",
        ia: (13, "Other"),
        album: (11, "Other"),
        aa: (21, "Other"),
        composer: None,
        genre: (32, "Pop"),
        year: 2003,
    },
    Spec {
        pid: 5,
        title: "Track 5",
        ia: (12, "Survivor"),
        album: (13, "Third"),
        aa: (20, "Artist"),
        composer: None,
        genre: (31, "Rock"),
        year: 2004,
    },
];

/// Applied to both the before and the after database.
pub(crate) fn base_sql() -> String {
    let mut sql = String::new();
    for name in NAMES {
        sql += &format!(
            "INSERT INTO sort_map(name,name_order,name_section,sort_key)VALUES('{name}',{},12,X'{}');",
            base_order(name),
            sort_key_hex(name)
        );
    }
    sql += "INSERT INTO item(item_pid,media_type,album_pid,album_artist_pid,item_artist_pid,base_location_id,is_compilation)VALUES(5,8,13,20,12,1,0);
        INSERT INTO item_extra(item_pid,title,sort_title,location,file_size,total_time_ms)VALUES(5,'Track 5','Track 5','T5.m4a',100,1000);
        INSERT INTO item_store(item_pid,sync_id)VALUES(5,1005);
        INSERT INTO item_stats(item_pid)VALUES(5);INSERT INTO item_playback(item_pid)VALUES(5);
        INSERT INTO item_search(item_pid)VALUES(5);INSERT INTO item_video(item_pid)VALUES(5);
        INSERT INTO chapter(item_pid)VALUES(5);INSERT INTO lyrics(item_pid)VALUES(5);
        INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(5,5,0,0,0);";
    for (pid, name, rep) in [(11, "Target", 1), (12, "Survivor", 2), (13, "Other", 3)] {
        sql += &format!(
            "UPDATE item_artist SET sort_item_artist='{name}',grouping_key=X'{}',representative_item_pid={rep} WHERE item_artist_pid={pid};",
            gk_hex(name)
        );
    }
    for (pid, name, rep, order) in [(20, "Artist", 1, 0), (21, "Other", 3, 0)] {
        let order = base_order(name) + order;
        sql += &format!(
            "UPDATE album_artist SET sort_album_artist='{name}',grouping_key=X'{}',representative_item_pid={rep},sort_order={order},sort_order_section=12,name_order={order} WHERE album_artist_pid={pid};",
            gk_hex(name)
        );
    }
    for (pid, name, rep, year) in [(10, "Album", 2, 2001), (11, "Other", 3, 2003)] {
        sql += &format!(
            "UPDATE album SET sort_album='{name}',grouping_key=X'{}',representative_item_pid={rep},album_year={year},all_compilations=0 WHERE album_pid={pid};",
            gk_hex(name)
        );
    }
    for (pid, name, parent, rep, sync, year) in [
        (12, "Second", 20, 0, 112, 0),
        (13, "Third", 20, 5, 113, 2004),
    ] {
        sql += &format!(
            "INSERT INTO album(album_pid,album,sort_album,album_artist_pid,representative_item_pid,grouping_key,album_year,sync_id,album_type,feed_url)VALUES({pid},'{name}','{name}',{parent},{rep},X'{}',{year},{sync},1,'');",
            gk_hex(name)
        );
    }
    for (pid, name, rep) in [(31, "Rock", 1), (32, "Pop", 3)] {
        sql += &format!(
            "INSERT INTO genre(genre_id,genre,grouping_key,representative_item_pid)VALUES({pid},'{name}',X'{}',{rep});",
            gk_hex(name)
        );
    }
    sql += &format!(
        "INSERT INTO composer(composer_pid,composer,sort_composer,grouping_key,representative_item_pid)VALUES(41,'Comp A','Comp A',X'{}',2);",
        gk_hex("Comp A")
    );
    for s in &SPECS {
        let order = |n: &str| base_order(n);
        let composer = s.composer.map_or((0, 0, 0), |(pid, n)| (pid, order(n), 12));
        sql += &format!(
            "UPDATE item SET album_pid={},album_artist_pid={},item_artist_pid={},composer_pid={},genre_id={},is_compilation=0,track_number={},disc_number=1,episode_sort_id={},title_order={},title_order_section=12,item_artist_order={},item_artist_order_section=12,album_order={},album_order_section=12,album_artist_order={},album_artist_order_section=12,composer_order={},composer_order_section={},genre_order={},genre_order_section=12 WHERE item_pid={};",
            s.album.0,
            s.aa.0,
            s.ia.0,
            composer.0,
            s.genre.0,
            s.pid,
            s.pid,
            order(s.title),
            order(s.ia.1),
            order(s.album.1),
            order(s.aa.1),
            composer.1,
            composer.2,
            order(s.genre.1),
            s.pid
        );
        sql += &format!(
            "UPDATE item_extra SET sort_title=title,year={},copyright='(C) {}',track_count=9,disc_count=1,content_rating=0 WHERE item_pid={};",
            s.year, s.pid, s.pid
        );
        sql += &format!(
            "UPDATE item_search SET search_title={},search_album={},search_artist={},search_composer={},search_album_artist={} WHERE item_pid={};",
            order(s.title),
            order(s.album.1),
            order(s.ia.1),
            composer.1,
            order(s.aa.1),
            s.pid
        );
    }
    // Revisions of the group rows, below the library's highest revision 17.
    sql += "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES
        (6,10,0,4,0),(7,11,0,4,0),(8,12,0,4,0),(9,13,0,4,0),(10,20,0,7,0),(11,21,0,7,0),
        (12,11,0,2,0),(13,12,0,2,0),(14,13,0,2,0),(15,31,0,3,0),(16,32,0,3,0),(17,41,0,5,0);
        UPDATE _MLDatabaseProperties SET value='17' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
        UPDATE _MLDatabaseProperties SET value='10' WHERE key='MLSyncClientSyncAnchor';
        UPDATE _MLDatabaseProperties SET value='-5' WHERE key='MLSyncClientGenerationID';";
    // The receiver's item triggers keep item_state in step with its item
    // rows; touch them once so before and after start consistent.
    sql += "UPDATE item SET media_type=media_type;UPDATE item_extra SET title=title;UPDATE item_stats SET liked_state=liked_state;";
    sql
}

pub(crate) fn tables(path: &Path, out: &Path) -> Tables {
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

pub(crate) struct World {
    pub(crate) fixture: Fixture,
}
impl World {
    pub(crate) fn new() -> Self {
        let fixture = Fixture::new();
        fs::set_permissions(&fixture.dir, fs::Permissions::from_mode(0o700)).unwrap();
        Connection::open(&fixture.before)
            .unwrap()
            .execute_batch(&base_sql())
            .unwrap();
        fs::copy(&fixture.before, &fixture.after).unwrap();
        Self { fixture }
    }
    pub(crate) fn before(&self) -> Tables {
        tables(&self.fixture.before, &self.fixture.dir.join("b"))
    }
    /// Apply the phone's effect to the after database, then read both.
    pub(crate) fn after(&self, sql: &str) -> Tables {
        Connection::open(&self.fixture.after)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        tables(&self.fixture.after, &self.fixture.dir.join("a"))
    }
}

/// Builds the SQL of the phone's effect: sort_map rows for new names and
/// revisions in allocation order.
pub(crate) struct Phone {
    sql: String,
    names: BTreeMap<String, (i64, i64)>,
    next_order: i64,
    revision: i64,
}
impl Phone {
    pub(crate) fn new() -> Self {
        Self {
            sql: String::new(),
            names: NAMES
                .iter()
                .map(|n| ((*n).to_owned(), (base_order(n), 12)))
                .collect(),
            next_order: 500_000,
            revision: 17,
        }
    }
    /// Order and section of a name, adding the sort_map row the first time.
    pub(crate) fn name(&mut self, name: &str) -> (i64, i64) {
        if let Some(found) = self.names.get(name) {
            return *found;
        }
        self.next_order += 7;
        let order = (self.next_order, 12);
        self.sql += &format!(
            "INSERT INTO sort_map(name,name_order,name_section,sort_key)VALUES('{name}',{},12,X'{}');",
            order.0,
            sort_key_hex(name)
        );
        self.names.insert(name.to_owned(), order);
        order
    }
    pub(crate) fn sql(&mut self, sql: &str) -> &mut Self {
        self.sql += sql;
        self
    }
    /// A new revision row for a created group.
    pub(crate) fn revise_new(&mut self, class: i64, pid: i64) -> i64 {
        self.revision += 1;
        self.sql += &format!(
            "INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES({},{pid},0,{class},0);",
            self.revision
        );
        self.revision
    }
    /// A new revision for an existing entity.
    pub(crate) fn revise(&mut self, class: i64, pid: i64) -> i64 {
        self.revision += 1;
        self.sql += &format!(
            "UPDATE entity_revision SET revision={} WHERE entity_pid={pid} AND class={class} AND revision_type=0;",
            self.revision
        );
        self.revision
    }
    pub(crate) fn finish(mut self) -> String {
        self.sql += &format!("UPDATE item_stats SET play_count_recent=0,skip_count_recent=0;
            UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor';
            UPDATE _MLDatabaseProperties SET value='{0}' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
            UPDATE _MLDatabaseProperties SET value='-6' WHERE key='MLSyncClientGenerationID';", self.revision);
        self.sql
    }
}

pub(crate) fn text(value: &str) -> Value {
    Value::String(value.into())
}
pub(crate) fn num(value: i64) -> Value {
    Value::Integer(value.into())
}
pub(crate) fn update(sync_id: u64, fields: &[(&str, Value)]) -> TrackUpdate {
    TrackUpdate {
        sync_id,
        item: fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    }
}
pub(crate) fn operation(updates: &[TrackUpdate]) -> Vec<u8> {
    encode(
        &Envelope {
            database_pid: DATABASE as u64,
            primary_container_pid: CONTAINER as u64,
            revision: ANCHOR_BEFORE + 1,
            timestamp: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        },
        updates,
    )
    .unwrap()
}
fn failure(result: Result<MetadataEditReceipt>) -> String {
    format!("{:#}", result.expect_err("expected a refusal"))
}

const ALBUM_ID: i64 = 0x0ddb_a110_0000_0001;
const ARTIST_ID: i64 = 0x0ddb_a110_0000_0002;

/// The planner's simulation of a verified session leaves every item in the
/// groups the after state shows, with the same representatives.
fn assert_simulated(before: &Tables, after: &Tables, updates: &[TrackUpdate]) {
    let mut simulated = planning::Catalogue::read(before).unwrap();
    simulated
        .simulate(&planning::conform(before, updates).unwrap())
        .unwrap();
    assert_eq!(
        simulated.view(),
        planning::Catalogue::read(after).unwrap().view()
    );
}

#[test]
fn title_change_reaches_item_state_search_and_sort_map() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1002,
        &[
            ("title", text("Track Two")),
            ("sort_name", text("Two, Track")),
        ],
    )];
    let mut phone = Phone::new();
    let (shown, _) = phone.name("Track Two");
    let (sorted, section) = phone.name("Two, Track");
    phone.sql(&format!(
        "UPDATE item_extra SET title='Track Two',sort_title='Two, Track' WHERE item_pid=2;
         UPDATE item SET title_order={sorted},title_order_section={section} WHERE item_pid=2;
         UPDATE item_search SET search_title={shown} WHERE item_pid=2;"
    ));
    phone.revise(0, 2);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.updated(), 1);
    assert_eq!(receipt.highest_revision(), 18);
    assert_eq!(receipt.sort_map_additions, ["Track Two", "Two, Track"]);
    assert_eq!(receipt.updates[0].revisions.len(), 1);
    assert_eq!(receipt.property_changes[ANCHOR], ("10".into(), "11".into()));
    assert_eq!(
        receipt.recent_counter_resets.len(),
        3,
        "the fixture's three nonzero recent counters reset"
    );
}

#[test]
fn numbers_flags_and_copyright_change_exactly_their_cells() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1003,
        &[
            ("track_number", num(7)),
            ("track_count", num(12)),
            ("disc_number", num(2)),
            ("disc_count", num(3)),
            ("is_compilation", Value::Boolean(true)),
            ("content_rating", num(1)),
            ("copyright", text("(C) New")),
        ],
    )];
    let mut phone = Phone::new();
    phone.sql(
        "UPDATE item SET track_number=7,episode_sort_id=7,disc_number=2,is_compilation=1 WHERE item_pid=3;
         UPDATE item_extra SET track_count=12,disc_count=3,content_rating=1,copyright='(C) New' WHERE item_pid=3;",
    );
    phone.revise(0, 3);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.highest_revision(), 18);
    // The explicit-content flag follows content_rating through item_state.
    let state = after.get("item_state").unwrap();
    let row = state.one("item_pid", 3).unwrap();
    assert_eq!(typed::int(state, row, "has_explicit_content").unwrap(), 1);
}

/// A comment keeps its line breaks and tabs, and an empty one is stored as
/// `''`, as on the phone.
#[test]
fn a_comment_changes_exactly_its_cell() {
    for (stored, comment) in [
        ("NULL", "line one\nline two\ttabbed\r\nline three"),
        ("'Processed by SoX'", ""),
    ] {
        let world = World::new();
        for path in [&world.fixture.before, &world.fixture.after] {
            Connection::open(path)
                .unwrap()
                .execute_batch(&format!(
                    "UPDATE item_extra SET comment={stored} WHERE item_pid=3;"
                ))
                .unwrap();
        }
        let before = world.before();
        let updates = vec![update(1003, &[("comment", text(comment))])];
        let mut phone = Phone::new();
        phone.sql(&format!(
            "UPDATE item_extra SET comment='{comment}' WHERE item_pid=3;"
        ));
        phone.revise(0, 3);
        let after = world.after(&phone.finish());
        verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
        assert_simulated(&before, &after, &updates);
        // Another comment than the one sent refuses.
        let world = World::new();
        let mut phone = Phone::new();
        phone.sql("UPDATE item_extra SET comment='other' WHERE item_pid=3;");
        phone.revise(0, 3);
        let after = world.after(&phone.finish());
        assert!(verify(&world.before(), &after, &operation(&updates), ANCHOR_BEFORE).is_err());
    }
}

/// Seen on the phone: an album an insert batch killed mid-upload had left at
/// keep_local_status 0, outside the edit, went to 2; its revision moved one
/// above the session's own block and its stats revision row went.
#[test]
fn an_album_left_unevaluated_is_evaluated_above_the_session() {
    let evaluated = |album_sql: &str, revision: i64| {
        let world = World::new();
        for path in [&world.fixture.before, &world.fixture.after] {
            Connection::open(path)
                .unwrap()
                .execute_batch(
                    "UPDATE album SET keep_local_status=2;
                     UPDATE album SET keep_local_status=0 WHERE album_pid=13;
                     INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(18,13,0,4,1);",
                )
                .unwrap();
        }
        let before = world.before();
        let updates = vec![update(1003, &[("comment", text("New"))])];
        let mut phone = Phone::new();
        phone.revision = 18;
        phone.sql("UPDATE item_extra SET comment='New' WHERE item_pid=3;");
        assert_eq!(phone.revise(0, 3), 19);
        let mut sql = phone.finish();
        sql += &format!(
            "{album_sql}
             UPDATE entity_revision SET revision={revision} WHERE entity_pid=13 AND class=4 AND revision_type=0;
             DELETE FROM entity_revision WHERE revision=18;"
        );
        let after = world.after(&sql);
        verify(&before, &after, &operation(&updates), ANCHOR_BEFORE)
    };
    let receipt = evaluated(
        "UPDATE album SET keep_local_status=2 WHERE album_pid=13;",
        20,
    )
    .unwrap();
    assert_eq!(receipt.highest_revision(), 19);
    assert_eq!(
        receipt.keep_local_evaluations,
        [KeepLocalEvaluation {
            album_pid: 13,
            revision: (9, 20),
            removed_stats_revision: Some(18),
        }]
    );
    // Any other change to the album, or a revision left in place, refuses.
    assert!(
        failure(evaluated(
            "UPDATE album SET keep_local_status=2,album_year=1999 WHERE album_pid=13;",
            20
        ))
        .contains("unmodelled metadata edit rows")
    );
    assert!(
        failure(evaluated(
            "UPDATE album SET keep_local_status=2 WHERE album_pid=13;",
            9
        ))
        .contains("not above the session's block")
    );
}

/// As on the phone, 0 and an empty copyright clear their cells,
/// and a sole member's year 0 takes its album's year to 0.
#[test]
fn clearing_numbers_and_copyright_sets_zero_and_empty_text() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1005,
        &[
            ("year", num(0)),
            ("track_number", num(0)),
            ("track_count", num(0)),
            ("disc_count", num(0)),
            ("copyright", text("")),
        ],
    )];
    let mut phone = Phone::new();
    phone.sql(
        "UPDATE item SET track_number=0,episode_sort_id=0 WHERE item_pid=5;
         UPDATE item_extra SET year=0,track_count=0,disc_count=0,copyright='' WHERE item_pid=5;
         UPDATE album SET album_year=0 WHERE album_pid=13;",
    );
    phone.revise(4, 13);
    phone.revise(0, 5);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    let year = receipt.updates[0].album_year.as_ref().unwrap();
    assert_eq!((year.album_pid, year.before, year.after), (13, 2004, 0));
}

#[test]
fn year_change_recomputes_the_album_year_and_revises_the_album_first() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(1001, &[("year", num(2010))])];
    let mut phone = Phone::new();
    phone.sql("UPDATE item_extra SET year=2010 WHERE item_pid=1;UPDATE album SET album_year=2010 WHERE album_pid=10;");
    let album = phone.revise(4, 10);
    let item = phone.revise(0, 1);
    assert_eq!((album, item), (18, 19));
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    let year = receipt.updates[0].album_year.as_ref().unwrap();
    assert_eq!((year.album_pid, year.before, year.after), (10, 2001, 2010));
}

#[test]
fn year_change_leaves_the_album_year_when_another_member_is_newer() {
    let world = World::new();
    let before = world.before();
    // Track 2 (2001) is older than track 1's new year only after the change:
    // lowering track 2 to 1990 keeps the album at track 1's 2000.
    let updates = vec![update(1002, &[("year", num(1990))])];
    let mut phone = Phone::new();
    phone.sql("UPDATE item_extra SET year=1990 WHERE item_pid=2;UPDATE album SET album_year=2000 WHERE album_pid=10;");
    phone.revise(4, 10);
    phone.revise(0, 2);
    let after = world.after(&phone.finish());
    verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
}

#[test]
fn a_year_equal_to_the_albums_leaves_the_album_alone() {
    // As the phone did for 2021 -> 2022 in a 2022 album: only the item moves.
    let world = World::new();
    let before = world.before();
    let updates = vec![update(1001, &[("year", num(2001))])];
    let mut phone = Phone::new();
    phone.sql("UPDATE item_extra SET year=2001 WHERE item_pid=1;");
    assert_eq!(phone.revise(0, 1), 18);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert!(receipt.updates[0].album_year.is_none());
    // A phone that revised the album as well is not the model.
    let world = World::new();
    let before = world.before();
    let mut phone = Phone::new();
    phone.sql("UPDATE item_extra SET year=2001 WHERE item_pid=1;");
    phone.revise(4, 10);
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    assert!(verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).is_err());
}

#[test]
fn two_year_changes_in_one_album_revise_it_once() {
    // Groups take their numbers once per session, before any item (observed);
    // two updates touching one album are not observed apart.
    let world = World::new();
    let before = world.before();
    let updates = vec![
        update(1001, &[("year", num(2010))]),
        update(1002, &[("year", num(2005))]),
    ];
    let mut phone = Phone::new();
    phone.sql("UPDATE item_extra SET year=2010 WHERE item_pid=1;UPDATE item_extra SET year=2005 WHERE item_pid=2;UPDATE album SET album_year=2010 WHERE album_pid=10;");
    assert_eq!(phone.revise(4, 10), 18);
    assert_eq!(phone.revise(0, 1), 19);
    assert_eq!(phone.revise(0, 2), 20);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.highest_revision(), 20);
    let first = receipt.updates[0].album_year.as_ref().unwrap();
    let second = receipt.updates[1].album_year.as_ref().unwrap();
    assert_eq!((first.before, first.after), (2001, 2010));
    assert_eq!((second.before, second.after), (2010, 2010));
}

#[test]
fn joining_an_album_needs_the_same_compilation_grouping() {
    let world = World::new();
    Connection::open(&world.fixture.before)
        .unwrap()
        .execute_batch("UPDATE album SET all_compilations=1 WHERE album_pid=12;")
        .unwrap();
    let before = world.before();
    let join = |is_compilation: Option<bool>| {
        let mut fields = vec![
            ("album", text("Second")),
            ("sort_album", text("Second")),
            ("album_pid", num(112)),
        ];
        if let Some(flag) = is_compilation {
            fields.push(("is_compilation", Value::Boolean(flag)));
        }
        vec![update(1001, &fields)]
    };
    let error = format!("{:#}", check_conformant(&before, &join(None)).unwrap_err());
    assert!(error.contains("compilation grouping"), "{error}");
    // Becoming a compilation track in the same update matches the album.
    check_conformant(&before, &join(Some(true))).unwrap();
}

#[test]
fn album_move_to_a_new_album_creates_the_row_with_the_items_year() {
    let world = World::new();
    let before = world.before();
    // Track 2 is album 10's representative: the row hands over to track 1.
    let updates = vec![update(
        1002,
        &[
            ("album", text("Fresh")),
            ("sort_album", text("Fresh")),
            ("album_pid", num(ALBUM_ID)),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Fresh");
    phone.sql(&format!(
        "INSERT INTO album(album_pid,album,sort_album,album_artist_pid,representative_item_pid,grouping_key,album_year,sync_id,album_type,feed_url)
            VALUES(90,'Fresh','Fresh',20,2,X'{}',2001,{ALBUM_ID},1,'');
         UPDATE album SET representative_item_pid=1 WHERE album_pid=10;
         UPDATE item SET album_pid=90,album_order={order},album_order_section={section} WHERE item_pid=2;
         UPDATE item_search SET search_album={order} WHERE item_pid=2;",
        gk_hex("Fresh")
    ));
    phone.revise_new(4, 90);
    phone.revise(0, 2);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    let update = &receipt.updates[0];
    assert_eq!(update.groups[0].outcome, "created");
    assert_eq!(update.representatives.len(), 1);
    assert_eq!(
        (
            update.representatives[0].before,
            update.representatives[0].after
        ),
        (2, 1)
    );
    assert_eq!(
        update
            .revisions
            .iter()
            .map(|r| r.revision)
            .collect::<Vec<_>>(),
        [18, 19]
    );
}

#[test]
fn album_join_into_an_empty_row_makes_the_joiner_its_representative() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1001,
        &[
            ("album", text("Second")),
            ("sort_album", text("Second")),
            ("album_pid", num(112)),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Second");
    phone.sql(&format!(
        "UPDATE item SET album_pid=12,album_order={order},album_order_section={section} WHERE item_pid=1;
         UPDATE item_search SET search_album={order} WHERE item_pid=1;
         UPDATE album SET representative_item_pid=1 WHERE album_pid=12;"
    ));
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.updates[0].groups[0].outcome, "joined");
    // Joined groups get no revision of their own: only the item.
    assert_eq!(receipt.updates[0].revisions.len(), 1);
}

#[test]
fn album_join_into_a_populated_row_keeps_its_representative() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1001,
        &[
            ("album", text("Third")),
            ("sort_album", text("Third")),
            ("album_pid", num(113)),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Third");
    phone.sql(&format!(
        "UPDATE item SET album_pid=13,album_order={order},album_order_section={section} WHERE item_pid=1;
         UPDATE item_search SET search_album={order} WHERE item_pid=1;"
    ));
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
}

#[test]
fn artist_move_of_a_representative_creates_a_group_and_hands_over() {
    let world = World::new();
    let before = world.before();
    // Track 2 is the representative of artist 12 (members 2 and 5).
    let updates = vec![update(
        1002,
        &[
            ("artist", text("Newcomer")),
            ("sort_artist", text("Newcomer")),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Newcomer");
    phone.sql(&format!(
        "INSERT INTO item_artist(item_artist_pid,item_artist,sort_item_artist,grouping_key,representative_item_pid)
            VALUES(80,'Newcomer','Newcomer',X'{}',2);
         UPDATE item_artist SET representative_item_pid=5 WHERE item_artist_pid=12;
         UPDATE item SET item_artist_pid=80,item_artist_order={order},item_artist_order_section={section} WHERE item_pid=2;
         UPDATE item_search SET search_artist={order},search_album_artist={order} WHERE item_pid=2;",
        gk_hex("Newcomer")
    ));
    phone.revise_new(2, 80);
    phone.revise(0, 2);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.updates[0].representatives[0].after, 5);
}

#[test]
fn artist_move_of_a_sole_representative_joins_an_existing_group() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1001,
        &[
            ("artist", text("Survivor")),
            ("sort_artist", text("Survivor")),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Survivor");
    phone.sql(&format!(
        "UPDATE item_artist SET representative_item_pid=0 WHERE item_artist_pid=11;
         UPDATE item SET item_artist_pid=12,item_artist_order={order},item_artist_order_section={section} WHERE item_pid=1;
         UPDATE item_search SET search_artist={order},search_album_artist={order} WHERE item_pid=1;"
    ));
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
}

#[test]
fn a_join_that_would_rewrite_a_stored_name_is_refused() {
    let world = World::new();
    // The row 12 keeps its old grouping key but was renamed in place, as the
    // receiver does for a non-representative artist edit. Moving a
    // representative to the old name matches that key and would rewrite it.
    Connection::open(&world.fixture.before)
        .unwrap()
        .execute_batch("UPDATE item_artist SET item_artist='Renamed',sort_item_artist='Renamed' WHERE item_artist_pid=12;")
        .unwrap();
    fs::copy(&world.fixture.before, &world.fixture.after).unwrap();
    let before = world.before();
    let updates = vec![update(
        1001,
        &[
            ("artist", text("Survivor")),
            ("sort_artist", text("Survivor")),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Survivor");
    phone.sql(&format!(
        "UPDATE item_artist SET representative_item_pid=0 WHERE item_artist_pid=11;
         UPDATE item_artist SET item_artist='Survivor',sort_item_artist='Survivor' WHERE item_artist_pid=12;
         UPDATE item SET item_artist_pid=12,item_artist_order={order},item_artist_order_section={section} WHERE item_pid=1;
         UPDATE item_search SET search_artist={order},search_album_artist={order} WHERE item_pid=1;"
    ));
    phone.revise(2, 12);
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    let error = failure(verify(&before, &after, &operation(&updates), ANCHOR_BEFORE));
    assert!(
        error.contains("rewrite") || error.contains("merge"),
        "{error}"
    );
}

#[test]
fn two_representatives_moving_to_one_new_artist_end_with_the_lowest_member() {
    let world = World::new();
    let before = world.before();
    let updates = vec![
        update(
            1002,
            &[("artist", text("Joint")), ("sort_artist", text("Joint"))],
        ),
        update(
            1001,
            &[("artist", text("Joint")), ("sort_artist", text("Joint"))],
        ),
    ];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Joint");
    phone.sql(&format!(
        "INSERT INTO item_artist(item_artist_pid,item_artist,sort_item_artist,grouping_key,representative_item_pid)
            VALUES(81,'Joint','Joint',X'{}',1);
         UPDATE item_artist SET representative_item_pid=5 WHERE item_artist_pid=12;
         UPDATE item_artist SET representative_item_pid=0 WHERE item_artist_pid=11;
         UPDATE item SET item_artist_pid=81,item_artist_order={order},item_artist_order_section={section} WHERE item_pid IN(1,2);
         UPDATE item_search SET search_artist={order},search_album_artist={order} WHERE item_pid IN(1,2);",
        gk_hex("Joint")
    ));
    phone.revise_new(2, 81);
    phone.revise(0, 2);
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.updated(), 2);
    assert_eq!(receipt.updates[0].groups[0].outcome, "created");
    assert_eq!(receipt.updates[1].groups[0].outcome, "joined");
}

/// Seen on the phone: four updates that created an
/// artist, a genre and an album and revised another album's year. The phone
/// numbered every group first, by class, then the items in operation order;
/// the new album came before the revised one. `revisions` lists (class,
/// entity, new row) in the phone's order.
fn four_group_session(revisions: &[(i64, i64, bool)]) -> Result<MetadataEditReceipt> {
    let world = World::new();
    let before = world.before();
    let updates = vec![
        update(
            1002,
            &[
                ("artist", text("Newcomer")),
                ("sort_artist", text("Newcomer")),
            ],
        ),
        update(1003, &[("genre", text("Jazz"))]),
        update(1001, &[("year", num(2010))]),
        update(
            1005,
            &[
                ("album", text("Fresh")),
                ("sort_album", text("Fresh")),
                ("album_pid", num(ALBUM_ID)),
            ],
        ),
    ];
    let mut phone = Phone::new();
    let artist = phone.name("Newcomer");
    let jazz = phone.name("Jazz");
    let fresh = phone.name("Fresh");
    let (artist_key, jazz_key, fresh_key) = (gk_hex("Newcomer"), gk_hex("Jazz"), gk_hex("Fresh"));
    let (artist_order, artist_section) = artist;
    let (jazz_order, jazz_section) = jazz;
    let (fresh_order, fresh_section) = fresh;
    phone.sql(&format!(
        "INSERT INTO item_artist(item_artist_pid,item_artist,sort_item_artist,grouping_key,representative_item_pid)
            VALUES(80,'Newcomer','Newcomer',X'{artist_key}',2);
         UPDATE item_artist SET representative_item_pid=5 WHERE item_artist_pid=12;
         UPDATE item SET item_artist_pid=80,item_artist_order={artist_order},item_artist_order_section={artist_section} WHERE item_pid=2;
         UPDATE item_search SET search_artist={artist_order},search_album_artist={artist_order} WHERE item_pid=2;
         INSERT INTO genre(genre_id,genre,grouping_key,representative_item_pid)VALUES(60,'Jazz',X'{jazz_key}',3);
         UPDATE genre SET representative_item_pid=4 WHERE genre_id=32;
         UPDATE item SET genre_id=60,genre_order={jazz_order},genre_order_section={jazz_section} WHERE item_pid=3;
         UPDATE item_extra SET year=2010 WHERE item_pid=1;
         UPDATE album SET album_year=2010 WHERE album_pid=10;
         INSERT INTO album(album_pid,album,sort_album,album_artist_pid,representative_item_pid,grouping_key,album_year,sync_id,album_type,feed_url)
            VALUES(90,'Fresh','Fresh',20,5,X'{fresh_key}',2004,{ALBUM_ID},1,'');
         UPDATE album SET representative_item_pid=0 WHERE album_pid=13;
         UPDATE item SET album_pid=90,album_order={fresh_order},album_order_section={fresh_section} WHERE item_pid=5;
         UPDATE item_search SET search_album={fresh_order} WHERE item_pid=5;"
    ));
    for &(class, entity, new) in revisions {
        if new {
            phone.revise_new(class, entity);
        } else {
            phone.revise(class, entity);
        }
    }
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE)?;
    assert_simulated(&before, &after, &updates);
    Ok(receipt)
}

#[test]
fn a_session_numbers_every_group_before_its_items() {
    let items = [(0, 2, false), (0, 3, false), (0, 1, false), (0, 5, false)];
    let groups = [(2, 80, true), (3, 60, true)];
    // As on the phone, and with the two albums the other way round: the
    // order within one class is read from the after state.
    for albums in [
        [(4, 90, true), (4, 10, false)],
        [(4, 10, false), (4, 90, true)],
    ] {
        let order: Vec<_> = albums
            .iter()
            .chain(&groups)
            .chain(&items)
            .copied()
            .collect();
        let receipt = four_group_session(&order).unwrap();
        assert_eq!(receipt.highest_revision(), 25);
        assert_eq!(receipt.updates[3].revisions.last().unwrap().revision, 25);
    }
    // Each update's groups then its item, as the model once assumed.
    let interleaved = [
        (2, 80, true),
        (0, 2, false),
        (3, 60, true),
        (0, 3, false),
        (4, 10, false),
        (0, 1, false),
        (4, 90, true),
        (0, 5, false),
    ];
    assert!(four_group_session(&interleaved).is_err());
    // Classes out of order.
    let artist_first: Vec<_> = [(2, 80, true), (4, 90, true), (4, 10, false), (3, 60, true)]
        .iter()
        .chain(&items)
        .copied()
        .collect();
    assert!(four_group_session(&artist_first).is_err());
}

/// Track 3 moves to the existing genre Rock (a representative handover); track
/// 4 moves to a new genre and a new composer in one update. The order of the
/// two new groups' revisions was not observed, so both are accepted.
fn genre_and_composer_session(genre_first: bool) -> Result<MetadataEditReceipt> {
    let world = World::new();
    let before = world.before();
    let updates = vec![
        update(1003, &[("genre", text("Rock"))]),
        update(
            1004,
            &[
                ("genre", text("Jazz")),
                ("composer", text("Newcomposer")),
                ("sort_composer", text("Composer, New")),
            ],
        ),
    ];
    let mut phone = Phone::new();
    let rock = phone.name("Rock");
    let jazz = phone.name("Jazz");
    let composer_name = phone.name("Newcomposer");
    let composer_sort = phone.name("Composer, New");
    phone.sql(&format!(
        "UPDATE item SET genre_id=31,genre_order={},genre_order_section={} WHERE item_pid=3;
         UPDATE genre SET representative_item_pid=4 WHERE genre_id=32;
         INSERT INTO genre(genre_id,genre,grouping_key,representative_item_pid)VALUES(60,'Jazz',X'{}',4);
         INSERT INTO composer(composer_pid,composer,sort_composer,grouping_key,representative_item_pid)VALUES(61,'Newcomposer','Composer, New',X'{}',4);
         UPDATE genre SET representative_item_pid=0 WHERE genre_id=32;
         UPDATE item SET genre_id=60,genre_order={},genre_order_section={},composer_pid=61,composer_order={},composer_order_section={} WHERE item_pid=4;
         UPDATE item_search SET search_composer={} WHERE item_pid=4;",
        rock.0, rock.1, gk_hex("Jazz"), gk_hex("Newcomposer"), jazz.0, jazz.1, composer_sort.0, composer_sort.1, composer_name.0
    ));
    if genre_first {
        phone.revise_new(3, 60);
        phone.revise_new(5, 61);
    } else {
        phone.revise_new(5, 61);
        phone.revise_new(3, 60);
    }
    phone.revise(0, 3);
    phone.revise(0, 4);
    let after = world.after(&phone.finish());
    verify(&before, &after, &operation(&updates), ANCHOR_BEFORE)
}

#[test]
fn genre_and_composer_changes_move_by_name() {
    for genre_first in [false, true] {
        let receipt = genre_and_composer_session(genre_first).unwrap();
        assert_eq!(receipt.updated(), 2);
        assert_eq!(receipt.highest_revision(), 21);
        assert_eq!(receipt.updates[0].groups[0].outcome, "joined");
        assert_eq!(receipt.updates[0].representatives[0].after, 4);
        assert_eq!(receipt.updates[1].groups.len(), 2);
    }
}

#[test]
fn album_artist_change_with_the_full_set_regroups_all_three_levels() {
    let world = World::new();
    let before = world.before();
    // Track 1 is the representative of artist 11 and album artist 20.
    let updates = vec![update(
        1001,
        &[
            ("artist", text("Target")),
            ("sort_artist", text("Target")),
            ("album", text("Album")),
            ("sort_album", text("Album")),
            ("album_artist", text("Fresh Artist")),
            ("sort_album_artist", text("Fresh Artist")),
            ("album_pid", num(ALBUM_ID)),
            ("artist_pid", num(ARTIST_ID)),
        ],
    )];
    let mut phone = Phone::new();
    let (order, section) = phone.name("Fresh Artist");
    phone.sql(&format!(
        "INSERT INTO album_artist(album_artist_pid,album_artist,sort_album_artist,grouping_key,representative_item_pid,sync_id,sort_order,sort_order_section,name_order)
            VALUES(91,'Fresh Artist','Fresh Artist',X'{}',1,{ARTIST_ID},{order},{section},{order});
         INSERT INTO album(album_pid,album,sort_album,album_artist_pid,representative_item_pid,grouping_key,album_year,sync_id,album_type,feed_url)
            VALUES(92,'Album','Album',91,1,X'{}',2000,{ALBUM_ID},1,'');
         UPDATE album_artist SET representative_item_pid=2 WHERE album_artist_pid=20;
         UPDATE item SET album_artist_pid=91,album_artist_order={order},album_artist_order_section={section},album_pid=92 WHERE item_pid=1;
         UPDATE item_search SET search_album_artist={order} WHERE item_pid=1;",
        gk_hex("Fresh Artist"),
        gk_hex("Album")
    ));
    phone.revise_new(7, 91);
    phone.revise_new(4, 92);
    phone.revise(0, 1);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    let outcomes: Vec<_> = receipt.updates[0]
        .groups
        .iter()
        .map(|g| (g.class, g.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("album_artist", "created"),
            ("album", "created"),
            ("item_artist", "unchanged")
        ]
    );
}

#[test]
fn unmodelled_phone_behaviour_is_refused() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1002,
        &[
            ("title", text("Track Two")),
            ("sort_name", text("Track Two")),
        ],
    )];
    let good = |phone: &mut Phone| {
        let (order, section) = phone.name("Track Two");
        phone.sql(&format!(
            "UPDATE item_extra SET title='Track Two',sort_title='Track Two' WHERE item_pid=2;
             UPDATE item SET title_order={order},title_order_section={section} WHERE item_pid=2;
             UPDATE item_search SET search_title={order} WHERE item_pid=2;"
        ));
        phone.revise(0, 2);
    };
    // Control: the unmodified effect verifies.
    let mut phone = Phone::new();
    good(&mut phone);
    let after = world.after(&phone.finish());
    verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    let tampers = [
        (
            "another item changed",
            "UPDATE item_extra SET copyright='x' WHERE item_pid=3;",
        ),
        (
            "title order wrong",
            "UPDATE item SET title_order=title_order+1 WHERE item_pid=2;",
        ),
        (
            "search order wrong",
            "UPDATE item_search SET search_title=0 WHERE item_pid=2;",
        ),
        (
            "item state stale",
            "UPDATE item_state SET title='stale' WHERE item_pid=2;",
        ),
        (
            "revision gap",
            "UPDATE entity_revision SET revision=revision+1 WHERE entity_pid=2 AND class=0;",
        ),
        (
            "group revised",
            "UPDATE entity_revision SET revision=19 WHERE entity_pid=10 AND class=4;",
        ),
        (
            "anchor off",
            "UPDATE _MLDatabaseProperties SET value='12' WHERE key='MLSyncClientSyncAnchor';",
        ),
        (
            "last synced off",
            "UPDATE _MLDatabaseProperties SET value='19' WHERE key='MLSyncClientLastSyncedRevision';",
        ),
        (
            "hero unchanged",
            "UPDATE _MLDatabaseProperties SET value='17' WHERE key='MLArtistHeroImageImportDatabaseRevision';",
        ),
        (
            "generation kept",
            "UPDATE _MLDatabaseProperties SET value='-5' WHERE key='MLSyncClientGenerationID';",
        ),
        (
            "representative moved",
            "UPDATE album SET representative_item_pid=1 WHERE album_pid=10;",
        ),
        (
            "album year moved",
            "UPDATE album SET album_year=1999 WHERE album_pid=10;",
        ),
        (
            "extra sort row",
            "INSERT INTO sort_map(name,name_order,name_section,sort_key)VALUES('Extra',1,1,X'3001');",
        ),
        (
            "stats changed",
            "UPDATE item_stats SET play_count_user=play_count_user+1 WHERE item_pid=1;",
        ),
        ("item deleted", "DELETE FROM item_search WHERE item_pid=4;"),
    ];
    for (name, sql) in tampers {
        let world = World::new();
        let before = world.before();
        let mut phone = Phone::new();
        good(&mut phone);
        let mut script = phone.finish();
        script += sql;
        let after = world.after(&script);
        assert!(
            verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).is_err(),
            "{name}"
        );
    }
}

#[test]
fn recent_counters_may_stay_or_reset_but_not_grow() {
    let updates = vec![update(1002, &[("track_number", num(9))])];
    let run = |tail: &str| {
        let world = World::new();
        let before = world.before();
        let mut phone = Phone::new();
        phone.sql("UPDATE item SET track_number=9,episode_sort_id=9 WHERE item_pid=2;");
        phone.revise(0, 2);
        let script = phone.finish() + tail;
        let after = world.after(&script);
        verify(&before, &after, &operation(&updates), ANCHOR_BEFORE)
    };
    run("").unwrap();
    // Counters the phone did not reset are accepted too.
    run("UPDATE item_stats SET play_count_recent=17 WHERE item_pid=2;").unwrap();
    assert!(run("UPDATE item_stats SET play_count_recent=99 WHERE item_pid=2;").is_err());
}

#[test]
fn identity_and_anchor_mismatches_are_refused() {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(1002, &[("track_number", num(9))])];
    let mut phone = Phone::new();
    phone.sql("UPDATE item SET track_number=9,episode_sort_id=9 WHERE item_pid=2;");
    phone.revise(0, 2);
    let after = world.after(&phone.finish());
    let good = operation(&updates);
    verify(&before, &after, &good, ANCHOR_BEFORE).unwrap();
    assert!(verify(&before, &after, &good, ANCHOR_BEFORE + 1).is_err());
    assert!(verify(&before, &after, &good, ANCHOR_BEFORE - 1).is_err());
    let envelope = |database: i64, container: i64, revision: u32| {
        encode(
            &Envelope {
                database_pid: database as u64,
                primary_container_pid: container as u64,
                revision,
                timestamp: UNIX_EPOCH,
            },
            &updates,
        )
        .unwrap()
    };
    verify(
        &before,
        &after,
        &envelope(DATABASE, CONTAINER, 11),
        ANCHOR_BEFORE,
    )
    .unwrap();
    assert!(
        verify(
            &before,
            &after,
            &envelope(DATABASE, CONTAINER, 12),
            ANCHOR_BEFORE
        )
        .is_err()
    );
    assert!(verify(&before, &after, &envelope(-9, CONTAINER, 11), ANCHOR_BEFORE).is_err());
    assert!(verify(&before, &after, &envelope(DATABASE, -9, 11), ANCHOR_BEFORE).is_err());
    assert!(verify(&before, &after, b"bplist00 not a plist", ANCHOR_BEFORE).is_err());
    // The same bytes verified against an unchanged after state do not apply.
    assert!(verify(&before, &before, &good, ANCHOR_BEFORE).is_err());
}

/// Operations outside the recipe never reach the model.
#[test]
fn refusals_name_their_reason() {
    let world = World::new();
    let before = world.before();
    let cases: Vec<(&str, Vec<TrackUpdate>, &str)> = vec![
        (
            "artist edit of a non-representative track",
            vec![update(
                1005,
                &[
                    ("artist", text("Elsewhere")),
                    ("sort_artist", text("Elsewhere")),
                ],
            )],
            "representative",
        ),
        (
            "album text without album_pid",
            vec![update(
                1001,
                &[("album", text("Loose")), ("sort_album", text("Loose"))],
            )],
            "album_pid",
        ),
        (
            "album_pid alone that is not the current album",
            vec![update(1001, &[("album_pid", num(ALBUM_ID))])],
            "re-key",
        ),
        (
            "album artist text without the full set",
            vec![update(
                1001,
                &[
                    ("album_artist", text("Loose")),
                    ("sort_album_artist", text("Loose")),
                ],
            )],
            "full set",
        ),
        (
            "artist_pid alone",
            vec![update(1001, &[("artist_pid", num(ARTIST_ID))])],
            "re-keys",
        ),
        (
            "album artist change by a track that is not its artist's representative",
            vec![update(
                1002,
                &[
                    ("artist", text("Survivor")),
                    ("sort_artist", text("Survivor")),
                    ("album", text("Album")),
                    ("sort_album", text("Album")),
                    ("album_artist", text("Fresh Artist")),
                    ("sort_album_artist", text("Fresh Artist")),
                    ("album_pid", num(ALBUM_ID)),
                    ("artist_pid", num(ARTIST_ID)),
                ],
            )],
            "album artist group",
        ),
        (
            "artist_pid that belongs to another album artist",
            vec![update(
                1001,
                &[
                    ("artist", text("Target")),
                    ("sort_artist", text("Target")),
                    ("album", text("Album")),
                    ("sort_album", text("Album")),
                    ("album_artist", text("Brand New")),
                    ("sort_album_artist", text("Brand New")),
                    ("album_pid", num(ALBUM_ID)),
                    ("artist_pid", num(121)),
                ],
            )],
            "re-key or rename",
        ),
        (
            "album_pid of an existing album under a different name",
            vec![update(
                1001,
                &[
                    ("album", text("Renamed")),
                    ("sort_album", text("Renamed")),
                    ("album_pid", num(112)),
                ],
            )],
            "another album artist or name",
        ),
        (
            "album_pid of another album artist's album",
            vec![update(
                1001,
                &[
                    ("album", text("Other")),
                    ("sort_album", text("Other")),
                    ("album_pid", num(111)),
                ],
            )],
            "another album artist",
        ),
        (
            "id collision with a track id",
            vec![update(
                1001,
                &[
                    ("album", text("Fresh")),
                    ("sort_album", text("Fresh")),
                    ("album_pid", num(1004)),
                ],
            )],
            "already",
        ),
        (
            "duplicate of an existing album",
            vec![update(
                1001,
                &[
                    ("album", text("Second")),
                    ("sort_album", text("Second")),
                    ("album_pid", num(ALBUM_ID)),
                ],
            )],
            "already exists",
        ),
        (
            "year with an existing album join",
            vec![update(
                1001,
                &[
                    ("year", num(1999)),
                    ("album", text("Second")),
                    ("sort_album", text("Second")),
                    ("album_pid", num(112)),
                ],
            )],
            "year",
        ),
        (
            "unchanged value",
            vec![update(1001, &[("track_number", num(1))])],
            "already",
        ),
        (
            "unknown track",
            vec![update(4242, &[("track_number", num(5))])],
            "not in the catalogue",
        ),
        (
            "title without sort name",
            vec![update(1001, &[("title", text("Only"))])],
            "together",
        ),
        (
            "rewriting join by case",
            vec![update(1001, &[("genre", text("rock"))])],
            "case",
        ),
    ];
    for (name, updates, reason) in cases {
        let error = format!("{:#}", check_conformant(&before, &updates).expect_err(name));
        assert!(error.contains(reason), "{name}: {error}");
        // The verifier refuses the same operation before modelling it.
        let bytes = operation(&updates);
        assert!(
            verify(&before, &before, &bytes, ANCHOR_BEFORE).is_err(),
            "{name}"
        );
    }
    // Native no-op album_pid next to a title edit is accepted.
    check_conformant(
        &before,
        &[update(
            1001,
            &[
                ("title", text("Renamed")),
                ("sort_name", text("Renamed")),
                ("album_pid", num(110)),
            ],
        )],
    )
    .unwrap();
}

#[test]
fn empty_strings_never_decode() {
    let world = World::new();
    let before = world.before();
    let mut bytes = Vec::new();
    let mut item = plist::Dictionary::new();
    item.insert("genre".into(), text(""));
    let mut track = plist::Dictionary::new();
    track.insert("operation".into(), "update_track".into());
    track.insert("pid".into(), num(1001));
    track.insert("item".into(), Value::Dictionary(item));
    let mut db_info = plist::Dictionary::new();
    db_info.insert("primary_container_pid".into(), num(CONTAINER));
    let mut db = plist::Dictionary::new();
    db.insert("operation".into(), "update_db_info".into());
    db.insert("pid".into(), num(DATABASE));
    db.insert("db_info".into(), Value::Dictionary(db_info));
    let mut root = plist::Dictionary::new();
    root.insert("revision".into(), 11_u32.into());
    root.insert("timestamp".into(), Value::Date(UNIX_EPOCH.into()));
    root.insert(
        "operations".into(),
        Value::Array(vec![Value::Dictionary(db), Value::Dictionary(track)]),
    );
    Value::Dictionary(root)
        .to_writer_binary(&mut bytes)
        .unwrap();
    let error = failure(verify(&before, &before, &bytes, ANCHOR_BEFORE));
    assert!(error.contains("must not be empty"), "{error}");
}

#[test]
fn creating_an_album_for_a_compilation_track_is_refused() {
    let world = World::new();
    Connection::open(&world.fixture.before)
        .unwrap()
        .execute_batch("UPDATE item SET is_compilation=1 WHERE item_pid=1;")
        .unwrap();
    let before = world.before();
    let updates = vec![update(
        1001,
        &[
            ("album", text("Fresh")),
            ("sort_album", text("Fresh")),
            ("album_pid", num(ALBUM_ID)),
        ],
    )];
    let error = format!("{:#}", check_conformant(&before, &updates).unwrap_err());
    assert!(error.contains("compilation"), "{error}");
}

#[test]
fn planner_output_always_conforms_and_verifies() {
    // plan -> encode -> phone effect -> verify, for a mixed single session.
    let world = World::new();
    let before = world.before();
    let requests = vec![
        planning::EditRequest {
            track: 1005,
            title: Some("Five".into()),
            track_number: Some(3),
            ..Default::default()
        },
        planning::EditRequest {
            track: 1004,
            genre: Some("Rock".into()),
            ..Default::default()
        },
    ];
    let updates =
        planning::plan(&before, &requests, &mut || panic!("no fresh id is needed")).unwrap();
    assert_eq!(updates.len(), 2);
    let mut phone = Phone::new();
    let shown = phone.name("Five").0;
    let rock = phone.name("Rock");
    phone.sql(&format!(
        "UPDATE item_extra SET title='Five',sort_title='Five' WHERE item_pid=5;
         UPDATE item SET title_order={shown},title_order_section=12,track_number=3,episode_sort_id=3 WHERE item_pid=5;
         UPDATE item_search SET search_title={shown} WHERE item_pid=5;
         UPDATE item SET genre_id=31,genre_order={},genre_order_section={} WHERE item_pid=4;",
        rock.0, rock.1
    ));
    phone.revise(0, 5);
    phone.revise(0, 4);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_simulated(&before, &after, &updates);
    assert_eq!(receipt.updated(), 2);
}

/// After the edit: track 2, whose recent counter the session cleared, played
/// twice and track 3 was skipped once; the phone renumbered every live item
/// above the session's block, with statistics revisions for both.
const LATER_LISTENING: &str = "
    UPDATE item_stats SET play_count_user=101,play_count_recent=2,has_been_played=1,date_played=750282000.5 WHERE item_pid=2;
    UPDATE item_stats SET skip_count_user=4,skip_count_recent=1,date_skipped=750282100.5 WHERE item_pid=3;
    UPDATE entity_revision SET revision=revision+20 WHERE class=0 AND revision_type=0 AND deleted=0;
    INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(40,2,0,0,1),(41,3,0,0,1);";

/// The title edit of track 2 with `extra` after it, read by a later run.
fn later_title_edit(extra: &str) -> (World, Tables, Tables, Vec<TrackUpdate>) {
    let world = World::new();
    let before = world.before();
    let updates = vec![update(
        1002,
        &[
            ("title", text("Track Two")),
            ("sort_name", text("Two, Track")),
        ],
    )];
    let mut phone = Phone::new();
    let (shown, _) = phone.name("Track Two");
    let (sorted, section) = phone.name("Two, Track");
    phone.sql(&format!(
        "UPDATE item_extra SET title='Track Two',sort_title='Two, Track' WHERE item_pid=2;
         UPDATE item SET title_order={sorted},title_order_section={section} WHERE item_pid=2;
         UPDATE item_search SET search_title={shown} WHERE item_pid=2;"
    ));
    phone.revise(0, 2);
    let after = world.after(&format!("{}{extra}", phone.finish()));
    (world, before, after, updates)
}

#[test]
fn a_later_run_absorbs_listening_and_renumbering_after_the_edit() {
    let (_world, before, after, updates) = later_title_edit(LATER_LISTENING);
    assert!(verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).is_err());
    let receipt = verify_later(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_eq!(receipt.updated(), 1);
    let receipt = serde_json::to_value(&receipt).unwrap();
    let events: Vec<_> = receipt["between_runs_playback"]["completed_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["item_pid"].as_i64().unwrap())
        .collect();
    assert_eq!(events, [2, 3]);
    // Without listening a later run proves what the batch's own run does.
    let (_world, before, after, updates) = later_title_edit("");
    let same = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    let later = verify_later(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_eq!(later, same);
}

#[test]
fn a_later_run_refuses_what_listening_does_not_explain_after_the_edit() {
    for sql in [
        "UPDATE item_extra SET title='Renamed' WHERE item_pid=3",
        "UPDATE item_stats SET user_rating=80 WHERE item_pid=4",
        "UPDATE item_stats SET play_count_user=98 WHERE item_pid=2",
        "UPDATE entity_revision SET revision=50 WHERE class=3 AND entity_pid=31",
        // Not above the session's block: not the phone's renumbering.
        "UPDATE entity_revision SET revision=3 WHERE entity_pid=4 AND class=0 AND revision_type=0",
    ] {
        let (_world, before, after, updates) =
            later_title_edit(&format!("{LATER_LISTENING}{sql};"));
        assert!(
            verify_later(&before, &after, &operation(&updates), ANCHOR_BEFORE).is_err(),
            "{sql}"
        );
    }
}

/// Item 6, an entry the phone added itself (no sync ID, no file, its state at
/// the schema defaults), may have that state filled in by an edit session.
#[test]
fn an_edit_session_may_fill_in_the_state_of_an_entry_the_phone_added() {
    let world = World::new();
    for path in [&world.fixture.before, &world.fixture.after] {
        Connection::open(path)
            .unwrap()
            .execute_batch(
                "INSERT INTO item(item_pid,media_type)VALUES(6,8);
                INSERT INTO item_store(item_pid,sync_id)VALUES(6,0);",
            )
            .unwrap();
    }
    let before = world.before();
    let updates = vec![update(1003, &[("track_number", num(7))])];
    let mut phone = Phone::new();
    phone.sql(
        "UPDATE item SET track_number=7,episode_sort_id=7 WHERE item_pid=3;
        UPDATE item_state SET contained_persistent_id=NULL,lyrics_adam_id=NULL,is_valid_content_type=1 WHERE item_pid=6;",
    );
    phone.revise(0, 3);
    let after = world.after(&phone.finish());
    let receipt = verify(&before, &after, &operation(&updates), ANCHOR_BEFORE).unwrap();
    assert_eq!(receipt.filled_phone_entries, [6]);
}
