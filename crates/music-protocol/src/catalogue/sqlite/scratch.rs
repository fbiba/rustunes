//! Private catalogue copies: descriptors own the files; canonical names are
//! locators for SQLite, whose NOFOLLOW flag remains unchanged.
use super::{ReadControl, SCRATCH_SEQUENCE, check_control, copy_source_checked, unchanged};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, Metadata, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

const DB: &str = "MediaLibrary.sqlitedb";
const WAL: &str = "MediaLibrary.sqlitedb-wal";
const SHM: &str = "MediaLibrary.sqlitedb-shm";

pub(super) fn with_scratch<T>(
    parent: &Path,
    control: Option<&ReadControl>,
    operation: impl FnOnce(&mut Scratch) -> Result<T>,
) -> Result<T> {
    check_control(control)?;
    let mut scratch = Scratch::new(parent)?;
    // All operation errors, including SQL errors, pass the same final checks.
    // The Connection belongs to the callback and is gone before file cleanup.
    let result = operation(&mut scratch);
    let custody = scratch
        .final_check(control)
        .context("catalog scratch final custody check");
    match (result, custody) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(custody)) => Err(error.context(format!("{custody:#}"))),
    }
}

fn identity(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
}
fn named(parent: &File, name: &CString) -> Result<Option<libc::stat>> {
    let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(Some(unsafe { value.assume_init() }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(None)
    } else {
        Err(error.into())
    }
}
fn matches(metadata: &Metadata, value: &libc::stat) -> bool {
    metadata.dev() == value.st_dev as u64
        && metadata.ino() == value.st_ino as u64
        && metadata.mode() == value.st_mode as u32
        && metadata.uid() == value.st_uid
        && metadata.gid() == value.st_gid
}
fn stable_named(metadata: &Metadata, value: &libc::stat) -> bool {
    matches(metadata, value)
        && metadata.nlink() == value.st_nlink as u64
        && value.st_size >= 0
        && metadata.len() == value.st_size as u64
        && metadata.mtime() == value.st_mtime
        && metadata.mtime_nsec() == value.st_mtime_nsec
        && metadata.ctime() == value.st_ctime
        && metadata.ctime_nsec() == value.st_ctime_nsec
}
fn open_at(parent: &File, name: &CString, flags: i32, mode: u32) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    ensure!(
        fd >= 0,
        "open catalog scratch entry: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct Copy {
    name: CString,
    file: File,
    metadata: Metadata,
    complete: Option<([u8; 32], u64)>,
    mutable_cap: Option<u64>,
}
impl Copy {
    fn check(&self, directory: &File, control: Option<&ReadControl>, hash: bool) -> Result<()> {
        let held = self.file.metadata()?;
        ensure!(
            held.is_file() && identity(&self.metadata, &held),
            "held catalog scratch file identity changed"
        );
        let named = named(directory, &self.name)?;
        if self.mutable_cap.is_some() && named.is_none() {
            ensure!(
                held.nlink() == 0,
                "catalog scratch sidecar disappeared without unlink"
            );
        } else {
            let named = named.context("named catalog scratch file disappeared")?;
            ensure!(
                held.nlink() == 1 && named.st_nlink == 1 && matches(&held, &named),
                "named catalog scratch file identity changed"
            );
        }
        if let Some(cap) = self.mutable_cap {
            ensure!(held.len() <= cap, "catalog scratch sidecar exceeds bound");
        }
        if let Some((expected, bytes)) = self.complete {
            unchanged(&self.metadata, &held)?;
            ensure!(
                stable_named(&self.metadata, &named_at_required(directory, &self.name)?),
                "named catalog scratch bytes or metadata changed"
            );
            if hash {
                let mut digest = Sha256::new();
                let mut offset = 0;
                let mut buffer = [0; 65_536];
                while offset < bytes {
                    check_control(control)?;
                    let count = self
                        .file
                        .read_at(&mut buffer[..(bytes - offset).min(65_536) as usize], offset)?;
                    ensure!(count > 0, "held catalog scratch file shortened");
                    digest.update(&buffer[..count]);
                    offset += count as u64;
                }
                ensure!(
                    self.file.read_at(&mut buffer[..1], bytes)? == 0
                        && <[u8; 32]>::from(digest.finalize()) == expected,
                    "held catalog scratch bytes changed"
                );
                unchanged(&self.metadata, &self.file.metadata()?)?;
                let now = named_at_required(directory, &self.name)?;
                ensure!(
                    stable_named(&self.metadata, &now),
                    "catalog scratch file replaced during check"
                );
            }
        }
        Ok(())
    }
}
fn named_at_required(parent: &File, name: &CString) -> Result<libc::stat> {
    named(parent, name)?.context("catalog scratch entry disappeared")
}

pub(super) struct Scratch {
    input_parent: PathBuf,
    parent_path: PathBuf,
    parent: File,
    parent_metadata: Metadata,
    name: CString,
    path: PathBuf,
    directory: File,
    metadata: Metadata,
    copies: Vec<Copy>,
}
impl Scratch {
    fn new(input_parent: &Path) -> Result<Self> {
        let parent_path =
            fs::canonicalize(input_parent).context("locate catalog scratch parent")?;
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent_path)?;
        let parent_metadata = parent.metadata()?;
        ensure!(
            parent_metadata.is_dir() && identity(&parent_metadata, &fs::metadata(input_parent)?),
            "catalog scratch parent association changed"
        );
        for _ in 0..32 {
            let leaf = format!(
                "catalog-sqlite-{}-{}",
                std::process::id(),
                SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let name = CString::new(leaf.as_str())?;
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error).context("create private catalog scratch directory");
            }
            let directory = open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let metadata = directory.metadata()?;
            let value = Self {
                input_parent: input_parent.into(),
                path: parent_path.join(leaf),
                parent_path,
                parent,
                parent_metadata,
                name,
                directory,
                metadata,
                copies: Vec::new(),
            };
            ensure!(
                value.metadata.is_dir()
                    && value.metadata.mode() & 0o7777 == 0o700
                    && value.metadata.uid() == unsafe { libc::geteuid() },
                "catalog scratch directory is not private"
            );
            value.check_namespace()?;
            return Ok(value);
        }
        bail!("cannot allocate catalog scratch directory")
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    fn check_namespace(&self) -> Result<()> {
        ensure!(
            fs::canonicalize(&self.input_parent)? == self.parent_path
                && fs::canonicalize(&self.parent_path)? == self.parent_path,
            "catalog scratch parent locator changed"
        );
        for now in [
            self.parent.metadata()?,
            fs::symlink_metadata(&self.parent_path)?,
        ] {
            ensure!(
                now.is_dir() && identity(&self.parent_metadata, &now),
                "held or named catalog scratch parent changed"
            );
        }
        let held = self.directory.metadata()?;
        ensure!(
            held.is_dir() && identity(&self.metadata, &held),
            "held catalog scratch directory changed"
        );
        let named = named_at_required(&self.parent, &self.name)?;
        ensure!(
            matches(&self.metadata, &named)
                && fs::canonicalize(&self.path)? == self.path
                && identity(&held, &fs::symlink_metadata(&self.path)?),
            "named catalog scratch directory changed"
        );
        Ok(())
    }
    pub(super) fn check(&self, control: Option<&ReadControl>) -> Result<()> {
        self.check_namespace()?;
        for copy in &self.copies {
            copy.check(&self.directory, control, false)?;
        }
        check_control(control)?;
        self.check_namespace()
    }
    fn final_check(&self, control: Option<&ReadControl>) -> Result<()> {
        self.check(control)?;
        for copy in &self.copies {
            copy.check(&self.directory, control, true)?;
        }
        self.check(control)
    }
    fn create(&mut self, leaf: &str, control: Option<&ReadControl>) -> Result<usize> {
        ensure!(
            [DB, WAL, SHM].contains(&leaf),
            "unknown catalog scratch leaf"
        );
        self.check(control)?;
        let name = CString::new(leaf)?;
        let file = open_at(
            &self.directory,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let metadata = file.metadata()?;
        let index = self.copies.len();
        self.copies.push(Copy {
            name,
            file,
            metadata,
            complete: None,
            mutable_cap: None,
        });
        let metadata = &self.copies[index].metadata;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1 && metadata.mode() & 0o7777 == 0o600,
            "catalog scratch copy is not private"
        );
        Ok(index)
    }
    pub(super) fn copy(
        &mut self,
        source: &Path,
        leaf: &str,
        limit: u64,
        control: Option<&ReadControl>,
    ) -> Result<([u8; 32], u64)> {
        ensure!(
            [DB, WAL].contains(&leaf),
            "catalog scratch input leaf differs"
        );
        let index = self.create(leaf, control)?;
        let result = copy_source_checked(source, &mut self.copies[index].file, limit, control)?;
        self.copies[index].metadata = self.copies[index].file.metadata()?;
        self.copies[index].complete = Some(result);
        self.check(control)?;
        Ok(result)
    }
    pub(super) fn prepare_sql(
        &mut self,
        wal_bytes: Option<u64>,
        control: Option<&ReadControl>,
    ) -> Result<()> {
        let mut header = [0; 100];
        self.copies[0].file.read_exact_at(&mut header, 0)?;
        // A checkpointed WAL-mode database can have no captured WAL. SQLite
        // still creates an empty private WAL when reading its WAL-mode header.
        // Own that empty sidecar too; source hashes continue to record absence.
        if wal_bytes.is_none() && header[19] == 2 {
            let index = self.create(WAL, control)?;
            self.copies[index].mutable_cap = Some(0);
        }
        if wal_bytes.is_some() || header[19] == 2 {
            // SQLite may mutate/unlink SHM. Own it before opening SQL, so cleanup
            // never adopts a new sidecar by name. Bounds follow the WAL index layout.
            let raw = u16::from_be_bytes([header[16], header[17]]);
            let page = if raw == 1 { 65_536 } else { u64::from(raw) };
            ensure!(
                page >= 512 && page <= 65_536 && page.is_power_of_two(),
                "catalog scratch SHM page size invalid"
            );
            let frames = wal_bytes.unwrap_or(0).saturating_sub(32) / (page + 24);
            let regions = 1 + frames.saturating_sub(4062).div_ceil(4096);
            let os_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            ensure!(
                os_page > 0 && os_page <= 1 << 20 && (os_page as u64).is_power_of_two(),
                "catalog scratch SHM OS page size invalid"
            );
            let quantum = (os_page as u64).max(32_768);
            let cap = (regions * 32_768).div_ceil(quantum) * quantum;
            let index = self.create(SHM, control)?;
            self.copies[index].mutable_cap = Some(cap);
        }
        self.check(control)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        // Only positively matched files are unlinked through our held child.
        // A renamed directory is safe to clean; a replacement name is not ours.
        for copy in &self.copies {
            if let (Ok(held), Ok(Some(named))) =
                (copy.file.metadata(), named(&self.directory, &copy.name))
            {
                if identity(&copy.metadata, &held) && matches(&held, &named) && held.nlink() == 1 {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), copy.name.as_ptr(), 0);
                    }
                }
            }
        }
        if let Ok(Some(named)) = named(&self.parent, &self.name) {
            if matches(&self.metadata, &named) {
                unsafe {
                    libc::unlinkat(
                        self.parent.as_raw_fd(),
                        self.name.as_ptr(),
                        libc::AT_REMOVEDIR,
                    );
                }
            }
        }
    }
}
