//! What a copied `MediaLibrary.sqlitedb` says about the phone's sync history,
//! read without the catalogue's requirements: a phone no computer has synced
//! has no `db_info` row and no Media anchor, and `read_snapshot` refuses it.
//!
//! The phone's own Reset criterion decides which rows a computer put there:
//! `sync_id != 0` in `item_store`, `container`, `album` and `album_artist`
//! (docs/02 §10.5). Purchased, Apple Music and cloud items have
//! `sync_id` 0.
use super::*;
use rusqlite::OptionalExtension;

/// The tables whose `sync_id != 0` rows a computer's sync created.
pub const SYNCED_TABLES: [&str; 4] = ["item_store", "container", "album", "album_artist"];
const MAX_CONTAINERS: usize = 1024;

/// One playlist row, as far as the first sync needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerRow {
    pub container_pid: i64,
    pub sync_id: i64,
    pub name: String,
    pub is_hidden: bool,
    pub distinguished_kind: i64,
    pub smart: bool,
}

/// The sync state the database records. No Debug on the identities is
/// withheld here: database PIDs are not the library identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryState {
    pub user_version: i64,
    pub db_info_rows: usize,
    /// `db_pid` and `primary_container_pid` when exactly one row exists.
    pub db_info: Option<(i64, i64)>,
    /// `MLSyncClientSyncAnchor` exactly as stored; None when absent (the
    /// phone then reports Media anchor "0").
    pub media_anchor: Option<String>,
    /// Rows with `sync_id != 0`, per `SYNCED_TABLES` entry.
    pub synced_rows: [(&'static str, u64); 4],
    pub items: u64,
    pub containers: Vec<ContainerRow>,
}

impl LibraryState {
    /// No computer has synced this library: no `db_info` row, no anchor or
    /// anchor "0", and no row with a sync identity.
    pub fn never_synced(&self) -> bool {
        self.db_info_rows == 0
            && self
                .media_anchor
                .as_deref()
                .is_none_or(|anchor| anchor == "0")
            && self.synced_rows.iter().all(|(_, count)| *count == 0)
    }

    pub fn synced_row_total(&self) -> u64 {
        self.synced_rows.iter().map(|(_, count)| count).sum()
    }
}

/// Read the sync state from a stable capture: the database and, when the
/// caller observed one, its WAL. Same private copy, container checks and
/// read-only open as `read_snapshot`; the source must not change meanwhile.
pub fn read_library_state(
    database: &Path,
    wal: Option<&Path>,
    scratch_parent: &Path,
) -> Result<LibraryState> {
    scratch::with_scratch(scratch_parent, None, |scratch| {
        let db_path = scratch.path().join("MediaLibrary.sqlitedb");
        let wal_path = scratch.path().join("MediaLibrary.sqlitedb-wal");
        let db = scratch.copy(database, "MediaLibrary.sqlitedb", MAX_DATABASE_BYTES, None)?;
        let journal = wal
            .map(|p| scratch.copy(p, "MediaLibrary.sqlitedb-wal", MAX_WAL_BYTES, None))
            .transpose()?;
        validate_container(&db_path, journal.as_ref().map(|_| wal_path.as_path()))?;
        scratch.prepare_sql(journal.as_ref().map(|(_, bytes)| *bytes), None)?;
        scratch.check(None)?;
        let connection = open_readonly(&db_path)?;
        let state = query(&connection)?;
        drop(connection);
        scratch.check(None)?;
        ensure!(
            hash_file(database, MAX_DATABASE_BYTES)? == db,
            "source database changed during analysis"
        );
        if let (Some(source), Some(expected)) = (wal, journal) {
            ensure!(
                hash_file(source, MAX_WAL_BYTES)? == expected,
                "source WAL changed during analysis"
            );
        }
        Ok(state)
    })
}

fn query(connection: &Connection) -> Result<LibraryState> {
    connection.execute_batch("BEGIN DEFERRED")?;
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        user_version >= USER_VERSION_FLOOR,
        "unsupported MediaLibrary schema version {user_version}, older than {USER_VERSION_FLOOR}"
    );
    let count = |sql: &str| -> Result<u64> {
        let value: i64 = connection.query_row(sql, [], |r| r.get(0))?;
        u64::try_from(value).context("negative row count")
    };
    let db_info_rows = usize::try_from(count("SELECT count(*) FROM db_info")?)?;
    let db_info = if db_info_rows == 1 {
        Some(connection.query_row(
            "SELECT db_pid,primary_container_pid FROM db_info",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?)
    } else {
        None
    };
    let media_anchor = connection
        .query_row(
            "SELECT value FROM _MLDatabaseProperties WHERE key='MLSyncClientSyncAnchor'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if let Some(anchor) = &media_anchor {
        ensure!(
            anchor.len() <= 10 && anchor.bytes().all(|b| b.is_ascii_digit()),
            "persisted Media anchor is not unsigned decimal"
        );
    }
    let mut synced_rows = SYNCED_TABLES.map(|table| (table, 0));
    for (table, rows) in &mut synced_rows {
        *rows = count(&format!("SELECT count(*) FROM {table} WHERE sync_id != 0"))?;
    }
    let items = count("SELECT count(*) FROM item")?;
    let mut statement = connection.prepare(
        "SELECT container_pid,sync_id,name,is_hidden,distinguished_kind,smart_criteria IS NOT NULL \
         FROM container ORDER BY container_pid LIMIT ?1",
    )?;
    let containers = statement
        .query_map([MAX_CONTAINERS as i64 + 1], |r| {
            Ok(ContainerRow {
                container_pid: r.get(0)?,
                sync_id: r.get(1)?,
                name: r.get(2)?,
                is_hidden: r.get::<_, i64>(3)? != 0,
                distinguished_kind: r.get(4)?,
                smart: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        containers.len() <= MAX_CONTAINERS,
        "more than {MAX_CONTAINERS} playlists"
    );
    drop(statement);
    connection.execute_batch("COMMIT")?;
    Ok(LibraryState {
        user_version,
        db_info_rows,
        db_info,
        media_anchor,
        synced_rows,
        items,
        containers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::catalogue::Fixture;

    fn edited(sql: &str) -> (Fixture, LibraryState) {
        let fixture = Fixture::new(false);
        let connection = Connection::open(&fixture.database).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE container ADD COLUMN name TEXT NOT NULL DEFAULT '';
                 ALTER TABLE container ADD COLUMN is_hidden INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE container ADD COLUMN distinguished_kind INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE container ADD COLUMN smart_criteria BLOB;",
            )
            .unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        let state = read_library_state(&fixture.database, None, &fixture.directory.0).unwrap();
        (fixture, state)
    }

    #[test]
    fn a_synced_fixture_is_not_never_synced() {
        let (_fixture, state) = edited("");
        assert_eq!(state.db_info_rows, 1);
        assert!(state.db_info.is_some());
        assert!(state.media_anchor.is_some());
        assert!(state.synced_row_total() > 0);
        assert!(!state.never_synced());
    }

    #[test]
    fn an_emptied_library_without_db_info_or_anchor_is_never_synced() {
        let (_fixture, state) = edited(
            "DELETE FROM item; DELETE FROM item_extra; DELETE FROM item_store;
             DELETE FROM item_state; DELETE FROM item_stats; DELETE FROM album;
             DELETE FROM album_artist; DELETE FROM item_artist; DELETE FROM container;
             DELETE FROM db_info;
             DELETE FROM _MLDatabaseProperties WHERE key='MLSyncClientSyncAnchor';
             INSERT INTO container(container_pid,sync_id,name,distinguished_kind,smart_criteria)
                 VALUES (7,0,'Recently Added',0,x'00');",
        );
        assert_eq!(state.db_info_rows, 0);
        assert_eq!(state.db_info, None);
        assert_eq!(state.media_anchor, None);
        assert_eq!(state.items, 0);
        assert_eq!(state.synced_row_total(), 0);
        assert_eq!(
            state.containers,
            [ContainerRow {
                container_pid: 7,
                sync_id: 0,
                name: "Recently Added".into(),
                is_hidden: false,
                distinguished_kind: 0,
                smart: true,
            }]
        );
        assert!(state.never_synced());
    }

    #[test]
    fn any_sign_of_a_sync_is_not_never_synced() {
        let empty = "DELETE FROM item; DELETE FROM item_extra; DELETE FROM item_store;
             DELETE FROM item_state; DELETE FROM item_stats; DELETE FROM album;
             DELETE FROM album_artist; DELETE FROM container; DELETE FROM db_info;
             DELETE FROM _MLDatabaseProperties WHERE key='MLSyncClientSyncAnchor';";
        for extra in [
            "INSERT INTO db_info VALUES (5, 6);",
            "INSERT INTO _MLDatabaseProperties VALUES ('MLSyncClientSyncAnchor','3');",
            "INSERT INTO container(container_pid,sync_id) VALUES (9,9);",
            "INSERT INTO album_artist(album_artist_pid,sync_id) VALUES (9,4);",
        ] {
            let (_fixture, state) = edited(&format!("{empty}{extra}"));
            assert!(!state.never_synced(), "{extra}");
        }
        let (_fixture, zero) = edited(&format!(
            "{empty}INSERT INTO _MLDatabaseProperties VALUES ('MLSyncClientSyncAnchor','0');"
        ));
        assert_eq!(zero.media_anchor.as_deref(), Some("0"));
        assert!(zero.never_synced());
    }
}
