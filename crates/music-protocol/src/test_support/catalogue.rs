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
