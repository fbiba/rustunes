//! Creation-held SQL copies. This private callback creates no Before or native permit.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr},
    fs::{self, File, Metadata},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Component, Path, PathBuf},
};

const DB: &str = "database.sqlite";
const WAL: &str = "database.sqlite-wal";
const SHM: &str = "database.sqlite-shm";

fn combine<T>(result: Result<T>, final_result: Result<()>) -> Result<T> {
    match (result, final_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(final_error)) => Err(error.context(format!(
            "SQL custody final check also failed: {final_error:#}"
        ))),
    }
}

fn component(value: &OsStr) -> Result<CString> {
    ensure!(
        !value.is_empty() && value != "." && value != ".." && !value.as_bytes().contains(&b'/'),
        "invalid SQL custody component"
    );
    Ok(CString::new(value.as_bytes())?)
}
fn path(value: &Path) -> Result<()> {
    ensure!(
        value.is_absolute()
            && value.as_os_str().as_bytes().len() <= 4096
            && value
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && value.components().count() <= 128,
        "SQL custody requires bounded absolute canonical spelling"
    );
    ensure!(
        !value
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|byte| byte.is_ascii_control()),
        "SQL custody path contains control bytes"
    );
    let normalized: PathBuf = value.components().collect();
    ensure!(
        normalized.as_os_str() == value.as_os_str(),
        "SQL custody path spelling aliases its components"
    );
    Ok(())
}
fn private(metadata: &Metadata, directory: bool) -> Result<()> {
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == if directory { 0o700 } else { 0o600 }
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file() && metadata.nlink() == 1
            },
        "SQL custody entry must be private owned regular single-link file or directory"
    );
    Ok(())
}
fn open_directory(parent: &File, name: &CString) -> Result<File> {
    // SAFETY: a live parent and terminated component; the returned fd has one owner.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    ensure!(
        fd >= 0,
        "opening SQL custody directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn stat_at(parent: &File, name: &CString) -> Result<Option<libc::stat>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid directory fd, component, and stat allocation. No file is opened.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(unsafe { stat.assume_init() }))
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(error.into())
        }
    }
}
fn identity(metadata: &Metadata, named: &libc::stat) -> bool {
    metadata.dev() == named.st_dev as u64
        && metadata.ino() == named.st_ino as u64
        && metadata.mode() == named.st_mode as u32
        && metadata.uid() == named.st_uid
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn stable_named(metadata: &Metadata, named: &libc::stat) -> bool {
    identity(metadata, named)
        && metadata.nlink() == named.st_nlink as u64
        && metadata.len() == named.st_size as u64
        && metadata.mtime() == named.st_mtime
        && metadata.mtime_nsec() == named.st_mtime_nsec
        && metadata.ctime() == named.st_ctime
        && metadata.ctime_nsec() == named.st_ctime_nsec
}

struct Ancestor {
    file: File,
    metadata: Metadata,
    name: Option<CString>,
}
struct Root {
    ancestors: Vec<Ancestor>,
    name: CString,
    file: File,
    metadata: Metadata,
}

struct Leaf {
    name: CString,
    file: File,
    metadata: Metadata,
    hash: [u8; 32],
}
struct MutableShm {
    file: File,
    metadata: Metadata,
    cap: u64,
}
fn shm_cap(database: &[u8], wal: &[u8]) -> Result<u64> {
    ensure!(
        database.len() >= 100 && &database[..16] == b"SQLite format 3\0",
        "SQL SHM needs valid database header"
    );
    let raw = u16::from_be_bytes([database[16], database[17]]);
    let page = if raw == 1 { 65_536u64 } else { u64::from(raw) };
    ensure!(
        (512..=65_536).contains(&page) && page.is_power_of_two(),
        "SQL SHM page size invalid"
    );
    let frames = (wal.len() as u64).saturating_sub(32) / (page + 24);
    let regions = 1 + frames.saturating_sub(4062).div_ceil(4096);
    let size = regions
        .checked_mul(32_768)
        .context("SQL SHM size overflow")?;
    let os_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    ensure!(
        os_page > 0 && os_page <= 1 << 20 && (os_page as u64).is_power_of_two(),
        "SQL SHM OS mapping page size unsupported"
    );
    let quantum = (os_page as u64).max(32_768);
    size.div_ceil(quantum)
        .checked_mul(quantum)
        .context("SQL SHM rounding overflow")
}

pub(super) struct CreatedOutput {
    root: Root,
    database: Leaf,
    wal: Option<Leaf>,
    shm: Option<MutableShm>,
    finished: bool,
}
