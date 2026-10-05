//! Private SQLite databases that hold caches, never journals. Each is created
//! 0600 in a 0700 directory, commits in WAL mode with NORMAL sync (a crash may
//! lose the last entries, never mix them), and is recognised by its
//! application id and exactly its tables. A damaged or unfamiliar database is
//! set aside as `.unreadable` and rebuilt; any other failure, such as a lock
//! held past the timeout, is returned.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};

pub(crate) struct Schema {
    /// What messages call the cache, e.g. "fingerprint cache".
    pub label: &'static str,
    pub file: &'static str,
    pub application_id: i64,
    /// Each table's name and its exact `CREATE TABLE` statement.
    pub tables: &'static [(&'static str, &'static str)],
}

/// A database whose identity or tables differ from this build's.
#[derive(Debug)]
struct Unfamiliar(&'static str);
impl std::fmt::Display for Unfamiliar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unfamiliar {} database", self.0)
    }
}
impl std::error::Error for Unfamiliar {}

/// Open the cache database, creating it on first use.
pub(crate) fn open(directory: &Path, schema: &Schema) -> Result<Connection> {
    let label = schema.label;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| format!("creating {label}"))?;
    let path = directory.join(schema.file);
    match connect(&path, schema) {
        Err(error) if rebuildable(&error) => {
            set_aside(&path, label)?;
            connect(&path, schema)
        }
        opened => opened,
    }
}
fn rebuildable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<Unfamiliar>()
            || matches!(
                cause
                    .downcast_ref::<rusqlite::Error>()
                    .and_then(rusqlite::Error::sqlite_error_code),
                Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
            )
    })
}
fn connect(path: &Path, schema: &Schema) -> Result<Connection> {
    let label = schema.label;
    // Create the file private; SQLite gives its -wal and -shm files the same mode.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {label} database"))?;
    let mut c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    c.busy_timeout(Duration::from_secs(10))?;
    if !familiar(&c, schema)? {
        create(&mut c, schema)?;
    }
    let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    ensure!(mode == "wal", "{label} refused WAL mode");
    c.execute_batch("PRAGMA synchronous=NORMAL")?;
    for (name, _) in schema.tables {
        c.query_row(&format!("SELECT count(*) FROM {name}"), [], |r| {
            r.get::<_, i64>(0)
        })?;
    }
    Ok(c)
}
/// This build's cache: its application id and exactly its tables.
fn familiar(c: &Connection, schema: &Schema) -> Result<bool> {
    let mut tables = schema.tables.to_vec();
    tables.sort_unstable_by_key(|(name, _)| *name);
    Ok(application_id(c)? == schema.application_id
        && objects(c)? == tables.iter().map(|(_, sql)| *sql).collect::<Vec<_>>())
}
fn application_id(c: &Connection) -> Result<i64> {
    Ok(c.query_row("PRAGMA application_id", [], |r| r.get(0))?)
}
fn objects(c: &Connection) -> Result<Vec<String>> {
    Ok(c.prepare("SELECT sql FROM sqlite_schema ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
fn create(c: &mut Connection, schema: &Schema) -> Result<()> {
    let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another run may have created it while this one waited for the lock.
    if familiar(&tx, schema)? {
        return Ok(());
    }
    if application_id(&tx)? != 0 || !objects(&tx)?.is_empty() {
        return Err(Unfamiliar(schema.label).into());
    }
    for (_, sql) in schema.tables {
        tx.execute_batch(sql)?;
    }
    tx.execute_batch(&format!("PRAGMA application_id={};", schema.application_id))?;
    tx.commit()?;
    Ok(())
}
/// Keep the last damaged or unfamiliar generation for inspection; the cache
/// then rebuilds. Its -wal and -shm files would corrupt a new database.
fn set_aside(path: &Path, label: &str) -> Result<()> {
    let mut aside = path.as_os_str().to_owned();
    aside.push(".unreadable");
    fs::rename(path, &aside).with_context(|| format!("setting aside an unreadable {label}"))?;
    for suffix in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        match fs::remove_file(&side) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("removing an unreadable {label}'s log"));
            }
            _ => {}
        }
    }
    Ok(())
}
