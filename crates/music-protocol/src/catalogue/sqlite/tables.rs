//! Private, bounded SQLite materialization of a saved catalogue's full tables,
//! for before/after SQL checks.
//! SQLite sees only private copies. Every source and copy is hash rechecked.
//! This raw full-table reader is deliberately independent of the projected
//! `Catalog` reader: it reads rows the projection does not, under its own limits.
use super::SourceHashes;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{
    Connection, OpenFlags,
    config::DbConfig,
    hooks::{AuthAction, Authorization},
    limits::Limit,
    types::ValueRef,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub use crate::protocol::transfer::hex;

pub const DB_CAP: u64 = super::MAX_DATABASE_BYTES;
pub const WAL_CAP: u64 = super::MAX_WAL_BYTES;
const ENCODED_CAP: usize = 64 << 20;
/// `Snapshot::layout_hash` of the 45 tables observed on iOS 27.0.1 and 27.2.
pub const LAYOUT_SHA256: &str = "282f0a1114d01c831e691f47b5071743f530936c81efb3d98c891992dfecdab3";

#[derive(Clone, Copy)]
pub struct Capture<'a> {
    pub database: &'a Path,
    pub database_sha256: [u8; 32],
    pub wal: Option<(&'a Path, [u8; 32])>,
}
impl<'a> Capture<'a> {
    /// Bind a catalogue's source hashes to its private copies. A WAL path
    /// without a hash, or a hash without a WAL path, is refused.
    pub fn bound(database: &'a Path, wal: Option<&'a Path>, hashes: &SourceHashes) -> Result<Self> {
        Ok(Self {
            database,
            database_sha256: hashes.database,
            wal: match (wal, hashes.wal) {
                (Some(path), Some(hash)) => Some((path, hash)),
                (None, None) => None,
                _ => bail!("snapshot WAL presence differs from its catalogue"),
            },
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SqlHashes {
    pub database_sha256: String,
    pub database_bytes: u64,
    pub wal_sha256: Option<String>,
    pub wal_bytes: Option<u64>,
}
impl SqlHashes {
    pub fn matches(&self, h: &SourceHashes) -> bool {
        self.database_sha256 == hex(&h.database)
            && self.database_bytes == h.database_bytes
            && self.wal_sha256 == h.wal.map(|v| hex(&v))
            && self.wal_bytes == h.wal_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}
impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Null => ("null", Value::Null).serialize(s),
            Self::Integer(n) => ("integer", n).serialize(s),
            Self::Real(n) => ("real", format!("{n:016x}")).serialize(s),
            Self::Text(v) => ("text", v).serialize(s),
            Self::Blob(v) => ("blob", hex(v)).serialize(s),
        }
    }
}
impl Cell {
    pub fn integer(&self) -> Result<i64> {
        if let Self::Integer(v) = self {
            Ok(*v)
        } else {
            bail!("expected integer storage")
        }
    }
    pub fn text(&self) -> Result<&str> {
        if let Self::Text(v) = self {
            Ok(v)
        } else {
            bail!("expected text storage")
        }
    }
}
pub type Row = Vec<Cell>;
pub type Rows = BTreeMap<Row, u32>;
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub definitions: Vec<Vec<Value>>,
    pub rows: Rows,
}
impl Table {
    pub fn column(&self, n: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c == n)
            .with_context(|| format!("missing column {n}"))
    }
    pub fn at<'a>(&self, r: &'a Row, n: &str) -> Result<&'a Cell> {
        r.get(self.column(n)?).context("short row")
    }
    pub fn one(&self, key: &str, id: i64) -> Result<&Row> {
        let c = self.column(key)?;
        let mut rows = self.rows.iter().filter(|(r, _)| r[c] == Cell::Integer(id));
        let (row, count) = rows.next().with_context(|| format!("missing {key}={id}"))?;
        ensure!(*count == 1 && rows.next().is_none(), "nonunique {key}={id}");
        Ok(row)
    }
}
#[derive(Clone)]
pub struct Snapshot {
    pub schema: Vec<Vec<Value>>,
    pub tables: BTreeMap<String, Table>,
    pub version: i64,
    pub hashes: SqlHashes,
    pub source: OwnedCapture,
}
impl Snapshot {
    pub fn table(&self, n: &str) -> Result<&Table> {
        self.tables
            .get(n)
            .with_context(|| format!("missing table {n}"))
    }
    pub fn schema_hash(&self) -> Result<String> {
        Ok(hex(&Sha256::digest(serde_json::to_vec(&self.schema)?)))
    }
    /// SHA-256 of each table's name and `table_xinfo` column definitions.
    /// Indexes, triggers and user_version are left out, so an index-only
    /// release keeps it while a table or column change does not.
    pub fn layout_hash(&self) -> Result<String> {
        let layout: Vec<_> = self
            .tables
            .iter()
            .map(|(name, table)| (name, &table.definitions))
            .collect();
        Ok(hex(&Sha256::digest(serde_json::to_vec(&layout)?)))
    }
    /// At or above the user_version floor, with the table layout that the
    /// full-table models were written against.
    pub fn layout_qualified(&self) -> Result<bool> {
        Ok(self.version >= super::USER_VERSION_FLOOR && self.layout_hash()? == LAYOUT_SHA256)
    }
}
#[derive(Clone)]
pub struct OwnedCapture {
    database: PathBuf,
    wal: Option<PathBuf>,
}
pub struct Budget {
    deadline: Instant,
    input: u64,
    cancelled: Option<Arc<AtomicBool>>,
}
impl Budget {
    pub fn new() -> Self {
        Self::new_until(Instant::now() + Duration::from_secs(180))
    }
    pub fn new_until(deadline: Instant) -> Self {
        Self {
            deadline: deadline.min(Instant::now() + Duration::from_secs(180)),
            input: 0,
            cancelled: None,
        }
    }
    pub fn new_cancellable_until(deadline: Instant, cancelled: Arc<AtomicBool>) -> Self {
        let mut budget = Self::new_until(deadline);
        budget.cancelled = Some(cancelled);
        budget
    }
    /// Check the already established tighter SQL bound and original Arc.
    /// This observes no new deadline and changes no budget or policy.
    pub fn ensure_original_control(
        &self,
        original_deadline: Instant,
        original_cancelled: &Arc<AtomicBool>,
    ) -> Result<()> {
        ensure!(
            self.deadline <= original_deadline
                && self
                    .cancelled
                    .as_ref()
                    .is_some_and(|value| Arc::ptr_eq(value, original_cancelled)),
            "SQL Budget differs from original deadline bound or cancellation Arc"
        );
        self.check()
    }
    pub fn add_input(&mut self, n: u64) -> Result<()> {
        self.input = self.input.checked_add(n).context("input size overflow")?;
        ensure!(self.input <= 512 << 20, "combined SQL input bound exceeded");
        Ok(())
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self
                .cancelled
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed)),
            "SQL verification cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "SQL verification deadline exceeded"
        );
        Ok(())
    }
    fn check_phase(&self, deadline: Instant) -> Result<()> {
        self.check()?;
        ensure!(Instant::now() < deadline, "SQLite deadline exceeded");
        Ok(())
    }
}
fn install_progress(
    c: &Connection,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
) -> Result<()> {
    let mut steps = 0u64;
    c.progress_handler(
        1000,
        Some(move || {
            steps += 1000;
            steps > 50_000_000
                || Instant::now() >= deadline
                || cancelled
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
        }),
    )?;
    Ok(())
}
pub fn directory(p: &Path) -> Result<()> {
    let mut d = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        d.mode(0o700);
    }
    d.create(p)
        .context("create new private SQL evidence directory")?;
    File::open(p.parent().context("directory parent absent")?)?.sync_all()?;
    Ok(())
}
pub fn durable(p: &Path, bytes: &[u8]) -> Result<[u8; 32]> {
    durable_checked(p, bytes, None)
}
fn durable_checked(p: &Path, bytes: &[u8], budget: Option<&Budget>) -> Result<[u8; 32]> {
    if let Some(budget) = budget {
        budget.check()?;
    }
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o.open(p)?;
    if let Some(budget) = budget {
        for chunk in bytes.chunks(65_536) {
            budget.check()?;
            f.write_all(chunk)?;
            budget.check()?;
        }
    } else {
        f.write_all(bytes)?;
    }
    if let Some(budget) = budget {
        budget.check()?;
    }
    f.sync_all()?;
    if let Some(budget) = budget {
        budget.check()?;
    }
    File::open(p.parent().context("evidence parent absent")?)?.sync_all()?;
    if let Some(budget) = budget {
        budget.check()?;
    }
    let sha = Sha256::digest(bytes).into();
    if let Some(budget) = budget {
        budget.check()?;
    }
    Ok(sha)
}
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len() && a.modified().ok() == b.modified().ok()
    }
}
pub fn read(p: &Path, cap: u64, b: &Budget) -> Result<Vec<u8>> {
    b.check()?;
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut f = o.open(p)?;
    let initial = f.metadata()?;
    ensure!(
        initial.is_file() && initial.len() <= cap,
        "SQL input is not bounded regular file"
    );
    let mut v = Vec::new();
    let mut part = [0; 65536];
    loop {
        b.check()?;
        let n = f.read(&mut part)?;
        if n == 0 {
            break;
        }
        ensure!(
            v.len() as u64 + n as u64 <= cap,
            "SQL input grew beyond cap"
        );
        v.extend_from_slice(&part[..n]);
    }
    let named = fs::symlink_metadata(p)?;
    ensure!(
        named.is_file()
            && !named.file_type().is_symlink()
            && same(&initial, &f.metadata()?)
            && same(&initial, &named)
            && v.len() as u64 == initial.len(),
        "SQL input changed during read"
    );
    Ok(v)
}
fn absent(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => bail!("unexpected SQL sidecar {}", p.display()),
    }
}
fn sidecar(p: &Path, suffix: &str) -> PathBuf {
    let mut x = p.as_os_str().to_os_string();
    x.push(suffix);
    PathBuf::from(x)
}
fn sidecars(source: &OwnedCapture) -> Result<()> {
    absent(&sidecar(&source.database, "-journal"))?;
    if source.wal.is_none() {
        absent(&sidecar(&source.database, "-wal"))?;
    }
    Ok(())
}
pub fn recheck(s: &Snapshot, b: &Budget) -> Result<()> {
    recheck_accepted_source(&s.source, &s.hashes, b)
}
fn recheck_accepted_source(source: &OwnedCapture, hashes: &SqlHashes, b: &Budget) -> Result<()> {
    sidecars(source)?;
    let db = read(&source.database, DB_CAP, b)?;
    ensure!(
        db.len() as u64 == hashes.database_bytes
            && hex(&Sha256::digest(&db)) == hashes.database_sha256,
        "source database changed"
    );
    if let Some(p) = &source.wal {
        let v = read(p, WAL_CAP, b)?;
        ensure!(
            Some(v.len() as u64) == hashes.wal_bytes
                && Some(hex(&Sha256::digest(v))) == hashes.wal_sha256,
            "source WAL changed"
        );
    }
    Ok(())
}
fn cell(v: ValueRef<'_>) -> Result<Cell> {
    Ok(match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(n) => Cell::Integer(n),
        ValueRef::Real(n) => Cell::Real(n.to_bits()),
        ValueRef::Text(v) => {
            ensure!(v.len() <= 1 << 20, "SQL text cell exceeds bound");
            Cell::Text(std::str::from_utf8(v)?.to_owned())
        }
        ValueRef::Blob(v) => {
            ensure!(v.len() <= 1 << 20, "SQL blob cell exceeds bound");
            Cell::Blob(v.to_vec())
        }
    })
}
fn plain(v: ValueRef<'_>) -> Result<Value> {
    Ok(match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => n.into(),
        ValueRef::Text(v) => std::str::from_utf8(v)?.into(),
        _ => bail!("unexpected schema cell storage"),
    })
}
fn plain_rows(c: &Connection, sql: &str, cap: usize) -> Result<Vec<Vec<Value>>> {
    plain_rows_checked(c, sql, cap, None)
}
fn check_sql_phase(policy: Option<(&Budget, Instant)>) -> Result<()> {
    if let Some((budget, deadline)) = policy {
        budget.check_phase(deadline)?;
    }
    Ok(())
}
fn plain_rows_checked(
    c: &Connection,
    sql: &str,
    cap: usize,
    policy: Option<(&Budget, Instant)>,
) -> Result<Vec<Vec<Value>>> {
    check_sql_phase(policy)?;
    let mut s = c.prepare(sql)?;
    check_sql_phase(policy)?;
    let n = s.column_count();
    let mut r = s.query([])?;
    check_sql_phase(policy)?;
    let mut out = Vec::new();
    let mut encoded = 0usize;
    loop {
        check_sql_phase(policy)?;
        let row = r.next()?;
        check_sql_phase(policy)?;
        let Some(row) = row else {
            break;
        };
        ensure!(out.len() < cap, "schema row bound exceeded");
        let values = (0..n)
            .map(|i| {
                check_sql_phase(policy)?;
                plain(row.get_ref(i)?)
            })
            .collect::<Result<Vec<_>>>()?;
        encoded = encoded
            .checked_add(serde_json::to_vec(&values)?.len())
            .context("schema byte overflow")?;
        ensure!(encoded <= 4 << 20, "schema metadata exceeds byte bound");
        out.push(values);
        check_sql_phase(policy)?;
    }
    check_sql_phase(policy)?;
    Ok(out)
}
