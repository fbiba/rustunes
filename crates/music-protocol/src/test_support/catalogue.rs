//! Shared synthetic catalogue fixtures built through the public bounded reader.
//! Fixture schema is intentionally independent of the parser's accepted-schema table;
//! parser edits must not silently rewrite the expected synthetic input.
use crate::catalogue::sqlite::{Catalog, read_snapshot};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const USER_VERSION: i64 = 2_400_060;
const CREATE_TABLES: &[&str] = &[
    "CREATE TABLE item(item_pid INTEGER PRIMARY KEY,media_type INTEGER,album_pid INTEGER,album_artist_pid INTEGER,item_artist_pid INTEGER,genre_id INTEGER,composer_pid INTEGER,base_location_id INTEGER,remote_location_id INTEGER,track_number INTEGER,disc_number INTEGER,is_compilation INTEGER,composer_order INTEGER);",
    "CREATE TABLE item_extra(item_pid INTEGER PRIMARY KEY,title TEXT,location TEXT,total_time_ms REAL,file_size INTEGER,track_count INTEGER,disc_count INTEGER,year INTEGER,copyright TEXT,content_rating INTEGER,content_rating_level INTEGER,sort_title TEXT,comment TEXT);",
    "CREATE TABLE item_store(item_pid INTEGER PRIMARY KEY,sync_id INTEGER);",
    "CREATE TABLE item_state(item_pid INTEGER PRIMARY KEY,is_placeholder INTEGER,is_library_added INTEGER,has_non_purgeable_asset INTEGER,is_supported_type INTEGER);",
    "CREATE TABLE item_stats(item_pid INTEGER PRIMARY KEY,play_count_user INTEGER,play_count_recent INTEGER,skip_count_user INTEGER,skip_count_recent INTEGER,has_been_played INTEGER,user_rating INTEGER,user_rating_is_derived INTEGER,liked_state INTEGER);",
    "CREATE TABLE album(album_pid INTEGER PRIMARY KEY,sync_id INTEGER,album TEXT,album_artist_pid INTEGER,all_compilations INTEGER,sort_album TEXT);",
    "CREATE TABLE album_artist(album_artist_pid INTEGER PRIMARY KEY,sync_id INTEGER,album_artist TEXT,sort_album_artist TEXT);",
    "CREATE TABLE item_artist(item_artist_pid INTEGER PRIMARY KEY,sync_id INTEGER,item_artist TEXT,sort_item_artist TEXT);",
    "CREATE TABLE base_location(base_location_id INTEGER PRIMARY KEY,path TEXT);",
    "CREATE TABLE genre(genre_id INTEGER PRIMARY KEY,sync_id INTEGER,genre TEXT);",
    "CREATE TABLE composer(composer_pid INTEGER PRIMARY KEY,sync_id INTEGER,composer TEXT,sort_composer TEXT);",
    "CREATE TABLE container(container_pid INTEGER PRIMARY KEY,sync_id INTEGER);",
    "CREATE TABLE db_info(db_pid INTEGER PRIMARY KEY,primary_container_pid INTEGER);",
    "CREATE TABLE _MLDatabaseProperties(key TEXT PRIMARY KEY,value TEXT);",
    "CREATE TABLE artwork(artwork_token TEXT,artwork_source_type INTEGER,artwork_type INTEGER,artwork_variant_type INTEGER,relative_path TEXT);",
    "CREATE TABLE artwork_token(entity_pid INTEGER,artwork_token TEXT,artwork_source_type INTEGER,artwork_type INTEGER,entity_type INTEGER,artwork_variant_type INTEGER);",
];

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub struct FixtureDirectory(pub PathBuf);
impl FixtureDirectory {
    fn new() -> Self {
        for _ in 0..32 {
            let path = std::env::temp_dir().join(format!(
                "catalogue-fixture-{}-{}",
                std::process::id(),
                FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create catalogue fixture directory: {error}"),
            }
        }
        panic!("cannot allocate catalogue fixture directory")
    }
}
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        // The fixture owns these exact files; no recursive cleanup of caller data.
        for name in [
            "MediaLibrary.sqlitedb",
            "MediaLibrary.sqlitedb-wal",
            "MediaLibrary.sqlitedb-shm",
        ] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

pub struct Fixture {
    pub directory: FixtureDirectory,
    pub database: PathBuf,
    pub connection: Connection,
}

impl Fixture {
    pub fn new(wal: bool) -> Self {
        let directory = FixtureDirectory::new();
        let database = directory.0.join("fixture.db");
        let connection = Connection::open(&database).unwrap();
        if wal {
            connection
                .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
                .unwrap();
        }
        connection
            .execute_batch(&format!("PRAGMA user_version={USER_VERSION};"))
            .unwrap();
        for table in CREATE_TABLES {
            connection.execute_batch(table).unwrap();
        }
        connection.execute_batch("INSERT INTO db_info VALUES(-10,-11);
            INSERT INTO _MLDatabaseProperties VALUES('MLSyncClientSyncAnchor','12');
            INSERT INTO album_artist VALUES(21,-21,'Synthetic Artist',NULL);
            INSERT INTO album_artist VALUES(22,-22,'Orphan Artist',NULL);
            INSERT INTO album VALUES(31,-31,'Synthetic Album',21,0,NULL);
            INSERT INTO album VALUES(32,-32,'Orphan Album',22,0,NULL);
            INSERT INTO item_artist VALUES(41,0,'Synthetic Artist',NULL);
            INSERT INTO base_location VALUES(51,'iTunes_Control/Music/F39');
            INSERT INTO genre VALUES(61,0,'Synthetic Genre');
            INSERT INTO composer VALUES(71,0,'Synthetic Composer','Synthetic Composer');
            INSERT INTO container VALUES(81,-11);
            INSERT INTO item VALUES(1,8,31,21,41,61,71,51,0,6,1,0,100);
            INSERT INTO item_extra VALUES(1,'Synthetic Title','TEST.m4a',147004.0,12345,10,0,2026,NULL,0,0,'Synthetic Title',NULL);
            INSERT INTO item_store VALUES(1,-1);
            INSERT INTO item_state VALUES(1,0,1,1,1);
            INSERT INTO item_stats VALUES(1,0,0,0,0,0,0,0,0);").unwrap();
        Self {
            directory,
            database,
            connection,
        }
    }
    pub fn read(&self, with_wal: bool) -> Result<Catalog> {
        let journal = self.database.with_file_name("fixture.db-wal");
        read_snapshot(
            &self.database,
            with_wal.then_some(journal.as_path()),
            &self.directory.0,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Connection closes after Drop; Unix permits unlinking our test files.
        for name in ["fixture.db", "fixture.db-wal", "fixture.db-shm"] {
            let _ = fs::remove_file(self.directory.0.join(name));
        }
    }
}

#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn copy_closed_collection_fixture(target: &Path) -> [u8; 32] {
    let mut fixture = Fixture::new(false);
    let connection = std::mem::replace(
        &mut fixture.connection,
        Connection::open_in_memory().unwrap(),
    );
    connection.close().unwrap();
    let bytes = fs::read(&fixture.database).unwrap();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .unwrap();
    std::io::Write::write_all(&mut file, &bytes).unwrap();
    file.sync_all().unwrap();
    Sha256::digest(bytes).into()
}

#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn collection_catalog(anchor: u32) -> Catalog {
    modified_collection_catalog(anchor, "")
}

/// Test-only composition of supported SQLite rows for caller policy checks.
#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn modified_collection_catalog(anchor: u32, sql: &str) -> Catalog {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute(
            "UPDATE _MLDatabaseProperties SET value=?1 WHERE key='MLSyncClientSyncAnchor'",
            [anchor.to_string()],
        )
        .unwrap();
    fixture.connection.execute_batch(sql).unwrap();
    fixture.read(false).unwrap()
}

/// Test-only complete raw catalogue for ordinary saved entrypoint checks.
#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn write_modified_collection_catalog(anchor: u32, sql: &str, output: &Path) -> Catalog {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute(
            "UPDATE _MLDatabaseProperties SET value=?1 WHERE key='MLSyncClientSyncAnchor'",
            [anchor.to_string()],
        )
        .unwrap();
    fixture.connection.execute_batch(sql).unwrap();
    let catalog = fixture.read(false).unwrap();
    fs::copy(&fixture.database, output).unwrap();
    catalog
}

#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn artwork_catalog(track_cache: u32, album_cache: u32) -> Catalog {
    artwork_catalog_typed(track_cache, album_cache, 6)
}

/// As `artwork_catalog`, with the track's own cached object typed
/// `track_type` when the album registers another cache (the phone types a
/// cover no album uses 1).
#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn artwork_catalog_typed(track_cache: u32, album_cache: u32, track_type: i64) -> Catalog {
    let fixture = Fixture::new(false);
    for cache in BTreeSet::from([track_cache, album_cache]) {
        let kind = if cache == album_cache { 6 } else { track_type };
        fixture.connection.execute("INSERT INTO artwork VALUES(?1,300,?2,0,'aa/00000000000000000000000000000000000000')",(cache.to_string(), kind)).unwrap();
    }
    fixture
        .connection
        .execute(
            "INSERT INTO artwork_token VALUES(1,?1,300,1,0,0)",
            [track_cache.to_string()],
        )
        .unwrap();
    fixture
        .connection
        .execute(
            "INSERT INTO artwork_token VALUES(31,?1,300,6,4,0)",
            [album_cache.to_string()],
        )
        .unwrap();
    fixture.read(false).unwrap()
}

#[allow(dead_code, reason = "each test crate uses part of this file")]
pub fn composer_transition_catalog(
    name: &str,
    sort: Option<&str>,
    order: i64,
    inserted: bool,
) -> Catalog {
    let fixture = Fixture::new(false);
    fixture
        .connection
        .execute(
            "UPDATE composer SET composer=?1,sort_composer=?2",
            rusqlite::params![name, sort],
        )
        .unwrap();
    fixture
        .connection
        .execute("UPDATE item SET composer_order=?1", [order])
        .unwrap();
    if inserted {
        fixture.connection.execute_batch("INSERT INTO item SELECT 2,media_type,album_pid,album_artist_pid,item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
            INSERT INTO item_extra SELECT 2,title,'NEW.m4a',total_time_ms,file_size,track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment FROM item_extra WHERE item_pid=1;
            INSERT INTO item_store VALUES(2,1000);
            INSERT INTO item_state SELECT 2,is_placeholder,is_library_added,has_non_purgeable_asset,is_supported_type FROM item_state WHERE item_pid=1;
            INSERT INTO item_stats SELECT 2,play_count_user,play_count_recent,skip_count_user,skip_count_recent,has_been_played,user_rating,user_rating_is_derived,liked_state FROM item_stats WHERE item_pid=1;
            UPDATE _MLDatabaseProperties SET value='13' WHERE key='MLSyncClientSyncAnchor';").unwrap();
    }
    fixture.read(false).unwrap()
}

const ITEM_TABLES: [&str; 10] = [
    "item",
    "item_extra",
    "item_store",
    "item_stats",
    "item_playback",
    "item_search",
    "item_state",
    "item_video",
    "chapter",
    "lyrics",
];
/// The phone's exact native schema, as `sqlite_schema` rows of type, name,
/// table name and SQL.
pub const NATIVE_SCHEMA: &str = include_str!("schema_fixture.json");
static NATIVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// The `sqlite_schema` row of iOS 27.2's one schema change (user_version
/// 2410000): an index.
const INDEX_ONLY_UPDATE: [&str; 4] = [
    "index",
    "AlbumCloudChannelName",
    "album",
    "CREATE INDEX AlbumCloudChannelName ON album (cloud_channel_name ASC)",
];
/// A before/after database pair in the phone's exact native schema
/// (`schema_fixture.json`; building it fails unless the database reproduces
/// that schema exactly) with four synthetic tracks, the after state having
/// lost track 1. For the models that read every table.
#[allow(dead_code, reason = "each test crate uses part of this file")]
pub struct NativeFixture {
    pub dir: PathBuf,
    pub before: PathBuf,
    pub after: PathBuf,
}
#[allow(dead_code, reason = "each test crate uses part of this file")]
impl NativeFixture {
    pub fn new() -> Self {
        Self::build(2_400_060, None)
    }
    /// As `new`, in the schema after iOS 27.2's index-only update.
    pub fn index_only_update() -> Self {
        Self::build(2_410_000, Some(INDEX_ONLY_UPDATE))
    }
    fn build(version: i64, index: Option<[&str; 4]>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "delete-sql-test-{}-{}",
            std::process::id(),
            NATIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let before = dir.join("before.sqlite");
        let c = Connection::open(&before).unwrap();
        let mut schema: Vec<Vec<Value>> = serde_json::from_str(NATIVE_SCHEMA).unwrap();
        if let Some(row) = index {
            schema.push(row.map(Value::from).to_vec());
            schema.sort_by(|a, b| {
                (a[0].as_str(), a[1].as_str()).cmp(&(b[0].as_str(), b[1].as_str()))
            });
        }
        for kind in ["table", "index", "view"] {
            for r in &schema {
                if r[0] == kind && r[1] != "sqlite_stat1" {
                    if let Some(sql) = r[3].as_str() {
                        c.execute_batch(sql).unwrap();
                    }
                }
            }
        }
        c.execute_batch(&format!("PRAGMA user_version={version};"))
            .unwrap();
        c.execute_batch("ANALYZE; DROP TABLE sqlite_stat4;
    INSERT INTO db_info(db_pid,primary_container_pid)VALUES(-2,-3);
    INSERT INTO base_location(base_location_id,path)VALUES(1,'iTunes_Control/Music/F00');
    INSERT INTO album_artist(album_artist_pid,album_artist,sync_id)VALUES(20,'Artist',120),(21,'Other',121);
    INSERT INTO album(album_pid,album,album_artist_pid,representative_item_pid,sync_id,all_compilations)VALUES(10,'Album',20,2,110,1),(11,'Other',21,3,111,0);
    INSERT INTO item_artist(item_artist_pid,item_artist,representative_item_pid)VALUES(11,'Target',1),(12,'Survivor',2),(13,'Other',3);
    INSERT INTO _MLDatabaseProperties(key,value)VALUES('MLSyncClientSyncAnchor','10'),('MLSyncClientLastSyncedRevision','4'),('MLSyncClientGenerationID','-5'),('MLArtistHeroImageImportDatabaseRevision','4');
  ").unwrap();
        for id in 1..=4i64 {
            let album = if id <= 2 { 10 } else { 11 };
            let artist = if id <= 2 { 20 } else { 21 };
            let ia = if id == 1 {
                11
            } else if id == 2 {
                12
            } else {
                13
            };
            c.execute("INSERT INTO item(item_pid,media_type,album_pid,album_artist_pid,item_artist_pid,base_location_id,is_compilation)VALUES(?1,8,?2,?3,?4,1,?5)",rusqlite::params![id,album,artist,ia,i64::from(id==2)]).unwrap();
            c.execute("INSERT INTO item_extra(item_pid,title,sort_title,location,file_size,total_time_ms)VALUES(?1,?2,?2,?3,100,1000)",rusqlite::params![id,format!("Track {id}"),format!("T{id}.m4a")]).unwrap();
            c.execute(
                "INSERT INTO item_store(item_pid,sync_id)VALUES(?1,?2)",
                rusqlite::params![id, id + 1000],
            )
            .unwrap();
            c.execute("INSERT INTO item_state(item_pid,persistent_id,is_library_added,has_non_purgeable_asset,is_supported_type)VALUES(?1,?1,1,1,1)",[id]).unwrap();
            for table in [
                "item_stats",
                "item_playback",
                "item_search",
                "item_video",
                "chapter",
                "lyrics",
            ] {
                c.execute(&format!("INSERT INTO {table}(item_pid)VALUES(?1)"), [id])
                    .unwrap();
            }
            c.execute("INSERT INTO entity_revision(revision,entity_pid,deleted,class,revision_type)VALUES(?1,?1,0,0,0)",[id]).unwrap();
        }
        c.execute_batch("UPDATE item_stats SET play_count_recent=5,play_count_user=5 WHERE item_pid=1; UPDATE item_stats SET play_count_recent=17,play_count_user=99 WHERE item_pid=2; UPDATE item_stats SET skip_count_recent=1,skip_count_user=3 WHERE item_pid=3;").unwrap();
        drop(c);
        let after = dir.join("after.sqlite");
        fs::copy(&before, &after).unwrap();
        let c = Connection::open(&after).unwrap();
        for table in ITEM_TABLES {
            c.execute(&format!("DELETE FROM {table} WHERE item_pid=1"), [])
                .unwrap();
        }
        c.execute_batch("UPDATE item_stats SET play_count_recent=0,skip_count_recent=0;
    UPDATE entity_revision SET revision=9,deleted=2 WHERE entity_pid=1;
    UPDATE _MLDatabaseProperties SET value='11' WHERE key='MLSyncClientSyncAnchor';
    UPDATE _MLDatabaseProperties SET value='9' WHERE key IN('MLSyncClientLastSyncedRevision','MLArtistHeroImageImportDatabaseRevision');
    UPDATE _MLDatabaseProperties SET value='6' WHERE key='MLSyncClientGenerationID';
    UPDATE album SET representative_item_pid=4 WHERE album_pid=11;
    UPDATE item_artist SET representative_item_pid=0 WHERE item_artist_pid=11;").unwrap();
        drop(c);
        for path in [&before, &after] {
            let c = Connection::open(path).unwrap();
            for row in &schema {
                if row[0] == "trigger" {
                    c.execute_batch(row[3].as_str().unwrap()).unwrap();
                }
            }
        }
        let c = Connection::open(&before).unwrap();
        let actual: Vec<Vec<Value>> = c
            .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
            .unwrap()
            .query_map([], |r| {
                Ok(vec![
                    r.get::<_, String>(0)?.into(),
                    r.get::<_, String>(1)?.into(),
                    r.get::<_, String>(2)?.into(),
                    r.get::<_, Option<String>>(3)?
                        .map_or(Value::Null, Value::String),
                ])
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            actual == schema,
            "synthetic fixture differs from exact native schema"
        );
        Self { dir, before, after }
    }
}
