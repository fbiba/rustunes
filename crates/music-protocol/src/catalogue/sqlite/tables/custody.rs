//! Creation-held SQL copies. This private callback creates no Before or native permit.
use super::{
    Budget, Capture, LoadedInput, OwnedCapture, Snapshot, SqlHashes, analyze_copies, load_input,
    recheck_accepted_source,
};
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

/// A borrowed view of real creation custody, never reconstructed from reports.
/// Owned Snapshot clones retain facts only, not these guards or this Budget.
pub struct Materialized<'a> {
    snapshot: &'a Snapshot,
    output: &'a CreatedOutput,
    budget: &'a Budget,
}
impl Materialized<'_> {
    pub fn snapshot(&self) -> &Snapshot {
        self.snapshot
    }
    pub fn budget(&self) -> &Budget {
        self.budget
    }
    pub fn recheck(&self) -> Result<()> {
        final_check(self.snapshot, self.output, self.budget)
    }
    /// Refuse a proposed path inside, or enclosing, this materialization's
    /// output directory, taken from the held custody rather than from the
    /// caller.
    pub fn ensure_disjoint_output(&self, proposed: &Path) -> Result<()> {
        self.recheck()?;
        let mut protected = PathBuf::from("/");
        for ancestor in &self.output.root.ancestors {
            if let Some(name) = &ancestor.name {
                protected.push(OsStr::from_bytes(name.to_bytes()));
            }
        }
        protected.push(OsStr::from_bytes(self.output.root.name.to_bytes()));
        ensure!(
            !proposed.starts_with(&protected) && !protected.starts_with(proposed),
            "proposed path overlaps creation-held SQL output"
        );
        Ok(())
    }
}

pub fn with_materialized_controlled<R>(
    input: Capture<'_>,
    out: &Path,
    budget: &mut Budget,
    visitor: impl for<'a> FnOnce(Materialized<'a>) -> Result<R>,
) -> Result<R> {
    run(input, out, budget, |_| Ok(()), |_, _, _| Ok(()), visitor)
}

fn run<R>(
    input: Capture<'_>,
    out: &Path,
    budget: &mut Budget,
    created: impl FnOnce(&CreatedOutput) -> Result<()>,
    live: impl FnOnce(&rusqlite::Connection, &CreatedOutput, &Budget) -> Result<()>,
    visitor: impl for<'a> FnOnce(Materialized<'a>) -> Result<R>,
) -> Result<R> {
    let (snapshot, output) = acquire(input, out, budget, created, live)?;
    let result = visitor(Materialized {
        snapshot: &snapshot,
        output: &output,
        budget,
    });
    // Callback refusal still closes all independent custody checks.
    combine(result, final_check(&snapshot, &output, budget))
}

fn acquire(
    input: Capture<'_>,
    out: &Path,
    budget: &mut Budget,
    created: impl FnOnce(&CreatedOutput) -> Result<()>,
    live: impl FnOnce(&rusqlite::Connection, &CreatedOutput, &Budget) -> Result<()>,
) -> Result<(Snapshot, CreatedOutput)> {
    ensure!(
        budget.cancelled.is_some(),
        "controlled SQL requires shared cancellation"
    );
    budget.check()?;
    let LoadedInput {
        source,
        db,
        wal,
        hashes,
    } = load_input(input, budget)?;
    let creation = CreatedOutput::create(out, input, &db, wal.as_deref(), budget);
    drop(db);
    drop(wal);
    let mut output = match creation {
        Ok(output) => output,
        Err(error) => {
            // Only accepted input facts exist here. Do not adopt incomplete
            // output custody or unlink an unknown partial creation prefix.
            let accepted = recheck_accepted_source(&source, &hashes, budget)
                .context("accepted SQL input final check");
            let control = budget.check().context("original SQL Budget final check");
            return combine(combine(Err(error), accepted), control);
        }
    };
    // Keep immutable accepted source/hash facts even if validation or analysis
    // refuses before a Snapshot exists. Only the cfg(test) entry supplies
    // nontrivial checkpoints, never a guard or reconstructed owner.
    let analysis = (|| {
        created(&output)?;
        output.check_live(budget)?;
        super::super::validate_container_with_control(
            &out.join(DB),
            input.wal.map(|_| out.join(WAL)).as_deref(),
            budget.deadline,
            budget
                .cancelled
                .as_ref()
                .context("controlled SQL flag absent")?
                .clone(),
        )?;
        output.check_live(budget)?;
        // The Connection is local to this helper and closes on every Result
        // path before any final peer reads or output descriptor release.
        analyze_copies(
            &out.join(DB),
            hashes.clone(),
            source.clone(),
            budget,
            true,
            Some(&output),
            |connection| live(connection, &output, budget),
        )
    })();
    // Close scratch exactly once, eagerly after the inner Connection is gone,
    // including preparation/analysis refusal. Expiry still refuses cleanup.
    let closed = output.finish_sql(budget);
    let result = (|| {
        let (snapshot, sql_deadline) = combine(analysis, closed)?;
        final_accepted_check(&source, &hashes, &output, budget)?;
        budget.check_phase(sql_deadline)?;
        Ok((snapshot, sql_deadline))
    })();
    // Every postcreation acquisition Result reaches all three independent
    // checks, including validation, analysis and previsitor phase errors. No retry or
    // renewed control occurs if scratch did not finish; its prefix is retained.
    let final_result = final_accepted_check(&source, &hashes, &output, budget);
    let (snapshot, sql_deadline) = combine(result, final_result)?;
    // Final closure work cannot silently move dispatch beyond the SQL phase.
    budget.check_phase(sql_deadline)?;
    Ok((snapshot, output))
}

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
fn final_check(snapshot: &Snapshot, output: &CreatedOutput, budget: &Budget) -> Result<()> {
    final_accepted_check(&snapshot.source, &snapshot.hashes, output, budget)
}
fn final_accepted_check(
    source: &OwnedCapture,
    hashes: &SqlHashes,
    output: &CreatedOutput,
    budget: &Budget,
) -> Result<()> {
    let accepted =
        recheck_accepted_source(source, hashes, budget).context("accepted SQL input final check");
    let copies = output
        .check_closed(budget)
        .context("creation-held SQL output final check");
    let control = budget.check().context("original SQL Budget final check");
    combine(combine(accepted, copies), control)
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
fn digest_held(file: &File, bytes: u64, budget: &Budget) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = [0; 65_536];
    while offset < bytes {
        budget.check()?;
        let count = (bytes - offset).min(buffer.len() as u64) as usize;
        let read = file.read_at(&mut buffer[..count], offset)?;
        ensure!(read > 0, "held SQL file was truncated");
        hash.update(&buffer[..read]);
        offset += read as u64;
        budget.check()?;
    }
    budget.check()?;
    ensure!(
        file.read_at(&mut buffer[..1], bytes)? == 0,
        "held SQL file grew"
    );
    budget.check()?;
    Ok(hash.finalize().into())
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
impl Root {
    fn create(out: &Path, budget: &Budget) -> Result<Self> {
        path(out)?;
        budget.check()?;
        let parent_path = out.parent().context("SQL custody parent absent")?;
        let name = component(out.file_name().context("SQL custody root name absent")?)?;
        let first = File::open("/")?;
        let metadata = first.metadata()?;
        let mut ancestors = vec![Ancestor {
            file: first,
            metadata,
            name: None,
        }];
        for part in parent_path.components() {
            budget.check()?;
            if let Component::Normal(part) = part {
                let name = component(part)?;
                let file = open_directory(&ancestors.last().unwrap().file, &name)?;
                let metadata = file.metadata()?;
                ensure!(metadata.is_dir(), "SQL custody ancestor is not directory");
                ancestors.push(Ancestor {
                    file,
                    metadata,
                    name: Some(name),
                });
            }
        }
        let parent = &ancestors.last().context("SQL custody parent missing")?.file;
        private(&parent.metadata()?, true)?;
        ensure!(
            stat_at(parent, &name)?.is_none(),
            "SQL custody output already exists"
        );
        budget.check()?;
        // mkdirat/openat is bracketed, not an atomic creation-and-open primitive.
        ensure!(
            unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0,
            "creating SQL custody root: {}",
            std::io::Error::last_os_error()
        );
        let file = open_directory(parent, &name)?;
        let metadata = file.metadata()?;
        private(&metadata, true)?;
        parent.sync_all()?;
        budget.check()?;
        let root = Self {
            ancestors,
            name,
            file,
            metadata,
        };
        root.check(budget)?;
        Ok(root)
    }
    fn check(&self, budget: &Budget) -> Result<()> {
        budget.check()?;
        for (index, ancestor) in self.ancestors.iter().enumerate() {
            budget.check()?;
            let held = ancestor.file.metadata()?;
            ensure!(
                held.is_dir()
                    && held.dev() == ancestor.metadata.dev()
                    && held.ino() == ancestor.metadata.ino()
                    && held.mode() == ancestor.metadata.mode()
                    && held.uid() == ancestor.metadata.uid(),
                "held SQL ancestor identity changed"
            );
            if let Some(name) = &ancestor.name {
                let named = stat_at(&self.ancestors[index - 1].file, name)?
                    .context("named SQL ancestor disappeared")?;
                ensure!(
                    identity(&held, &named),
                    "named SQL ancestor identity changed"
                );
            } else {
                let named = fs::symlink_metadata("/")?;
                ensure!(
                    held.dev() == named.dev() && held.ino() == named.ino(),
                    "named SQL root ancestor identity changed"
                );
            }
        }
        let parent = &self.ancestors.last().unwrap().file;
        private(&parent.metadata()?, true)?;
        let held = self.file.metadata()?;
        private(&held, true)?;
        ensure!(
            held.dev() == self.metadata.dev()
                && held.ino() == self.metadata.ino()
                && held.mode() == self.metadata.mode()
                && held.uid() == self.metadata.uid(),
            "held SQL output root identity changed"
        );
        let named = stat_at(parent, &self.name)?.context("named SQL output root disappeared")?;
        ensure!(
            identity(&held, &named),
            "named SQL output root identity changed"
        );
        budget.check()
    }
    fn create_file(&self, name: &str, budget: &Budget) -> Result<File> {
        self.check(budget)?;
        let name = component(OsStr::new(name))?;
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(
            fd >= 0,
            "creating exclusive SQL leaf: {}",
            std::io::Error::last_os_error()
        );
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn names(&self, budget: &Budget) -> Result<BTreeSet<String>> {
        self.check(budget)?;
        // Only a DIRECTORY descriptor is duplicated/closed, never a SQLite inode.
        let fd = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        ensure!(
            fd >= 0,
            "duplicating SQL directory: {}",
            std::io::Error::last_os_error()
        );
        let directory = unsafe { libc::fdopendir(fd) };
        if directory.is_null() {
            unsafe {
                libc::close(fd);
            }
            bail!(
                "opening held SQL directory stream: {}",
                std::io::Error::last_os_error()
            );
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Stream(directory);
        // A dup shares its directory offset. Rewind on EVERY enumeration.
        unsafe {
            libc::rewinddir(stream.0);
        }
        let mut names = BTreeSet::new();
        loop {
            budget.check()?;
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                ensure!(
                    std::io::Error::last_os_error().raw_os_error() == Some(0),
                    "enumerating held SQL directory: {}",
                    std::io::Error::last_os_error()
                );
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            ensure!(names.len() < 4, "unknown SQL custody namespace entries");
            let name = std::str::from_utf8(name).context("non-UTF8 SQL custody entry")?;
            ensure!(names.insert(name.to_owned()), "duplicate SQL custody entry");
        }
        self.check(budget)?;
        Ok(names)
    }
}

struct Leaf {
    name: CString,
    file: File,
    metadata: Metadata,
    hash: [u8; 32],
}
impl Leaf {
    fn create(root: &Root, name: &str, bytes: &[u8], budget: &Budget) -> Result<Self> {
        let mut file = root.create_file(name, budget)?;
        for chunk in bytes.chunks(65_536) {
            budget.check()?;
            file.write_all(chunk)?;
            budget.check()?;
        }
        budget.check()?;
        file.sync_all()?;
        root.file.sync_all()?;
        budget.check()?;
        let metadata = file.metadata()?;
        private(&metadata, false)?;
        ensure!(
            metadata.len() == bytes.len() as u64,
            "created SQL length differs"
        );
        let hash = Sha256::digest(bytes).into();
        let leaf = Self {
            name: component(OsStr::new(name))?,
            file,
            metadata,
            hash,
        };
        leaf.check(root, budget, true)?;
        Ok(leaf)
    }
    fn check(&self, root: &Root, budget: &Budget, hash: bool) -> Result<()> {
        budget.check()?;
        let held = self.file.metadata()?;
        private(&held, false)?;
        ensure!(
            stable(&self.metadata, &held),
            "held SQL leaf metadata changed"
        );
        let named = stat_at(&root.file, &self.name)?.context("named SQL leaf disappeared")?;
        ensure!(
            stable_named(&self.metadata, &named),
            "named SQL leaf identity changed"
        );
        if hash {
            ensure!(
                digest_held(&self.file, self.metadata.len(), budget)? == self.hash,
                "held SQL leaf SHA differs"
            );
        }
        ensure!(
            stable(&self.metadata, &self.file.metadata()?),
            "held SQL leaf changed during check"
        );
        let named = stat_at(&root.file, &self.name)?.context("named SQL leaf disappeared")?;
        ensure!(
            stable_named(&self.metadata, &named),
            "named SQL leaf changed during check"
        );
        budget.check()
    }
}
struct MutableShm {
    file: File,
    metadata: Metadata,
    cap: u64,
}
impl MutableShm {
    fn check(&self, root: &Root, budget: &Budget, allow_gone: bool) -> Result<bool> {
        budget.check()?;
        let held = self.file.metadata()?;
        ensure!(
            held.is_file()
                && held.uid() == self.metadata.uid()
                && held.mode() == self.metadata.mode()
                && held.dev() == self.metadata.dev()
                && held.ino() == self.metadata.ino()
                && held.len() <= self.cap,
            "owned SQL SHM identity or bound changed"
        );
        let named = stat_at(&root.file, &component(OsStr::new(SHM))?)?;
        match named {
            Some(named) => {
                ensure!(
                    held.nlink() == 1
                        && identity(&held, &named)
                        && named.st_nlink == 1
                        && named.st_size >= 0
                        && named.st_size as u64 <= self.cap,
                    "foreign or aliased SQL SHM"
                );
                Ok(true)
            }
            None => {
                ensure!(
                    allow_gone && held.nlink() == 0,
                    "owned SQL SHM disappeared without unlink"
                );
                Ok(false)
            }
        }
    }
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
impl CreatedOutput {
    fn create(
        out: &Path,
        input: Capture<'_>,
        db: &[u8],
        wal: Option<&[u8]>,
        budget: &Budget,
    ) -> Result<Self> {
        budget.check()?;
        path(out)?;
        let mut sources = Vec::new();
        for source in std::iter::once(input.database).chain(input.wal.map(|(p, _)| p)) {
            path(source)?;
            ensure!(
                fs::canonicalize(source)?.as_os_str() == source.as_os_str(),
                "SQL custody source ancestry aliases"
            );
            ensure!(
                !source.starts_with(out) && out != source,
                "SQL custody output overlaps source"
            );
            let metadata = fs::symlink_metadata(source)?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "SQL custody source is aliased or irregular"
            );
            let id = (metadata.dev(), metadata.ino());
            ensure!(!sources.contains(&id), "SQL custody input files alias");
            sources.push(id);
        }
        let root = Root::create(out, budget)?;
        let database = Leaf::create(&root, DB, db, budget)?;
        let wal_leaf = wal
            .map(|bytes| Leaf::create(&root, WAL, bytes, budget))
            .transpose()?;
        for leaf in std::iter::once(&database).chain(wal_leaf.as_ref()) {
            ensure!(
                !sources.contains(&(leaf.metadata.dev(), leaf.metadata.ino())),
                "SQL copy aliases input"
            );
        }
        if let Some(wal) = &wal_leaf {
            ensure!(
                (wal.metadata.dev(), wal.metadata.ino())
                    != (database.metadata.dev(), database.metadata.ino()),
                "SQL copies alias each other"
            );
        }
        let shm = wal
            .map(|bytes| -> Result<_> {
                let file = root.create_file(SHM, budget)?;
                let metadata = file.metadata()?;
                private(&metadata, false)?;
                ensure!(metadata.len() == 0, "new SQL SHM is not empty");
                file.sync_all()?;
                root.file.sync_all()?;
                budget.check()?;
                Ok(MutableShm {
                    file,
                    metadata,
                    cap: shm_cap(db, bytes)?,
                })
            })
            .transpose()?;
        let result = Self {
            root,
            database,
            wal: wal_leaf,
            shm,
            finished: false,
        };
        result.check_live(budget)?;
        Ok(result)
    }
    fn shape(&self, budget: &Budget, shm_present: bool) -> Result<()> {
        let mut expected = BTreeSet::from([DB.to_owned()]);
        if self.wal.is_some() {
            expected.insert(WAL.to_owned());
        }
        if shm_present {
            expected.insert(SHM.to_owned());
        }
        ensure!(
            self.root.names(budget)? == expected,
            "SQL custody namespace differs"
        );
        Ok(())
    }
    pub(super) fn check_live(&self, budget: &Budget) -> Result<()> {
        ensure!(!self.finished, "SQL custody live check after closure");
        self.root.check(budget)?;
        self.database.check(&self.root, budget, false)?;
        if let Some(wal) = &self.wal {
            wal.check(&self.root, budget, false)?;
        }
        if let Some(shm) = &self.shm {
            shm.check(&self.root, budget, false)?;
        }
        self.shape(budget, self.shm.is_some())?;
        budget.check()
    }
    fn finish_sql(&mut self, budget: &Budget) -> Result<()> {
        self.root.check(budget)?;
        self.database.check(&self.root, budget, true)?;
        if let Some(wal) = &self.wal {
            wal.check(&self.root, budget, true)?;
        }
        let shm_present = self
            .shm
            .as_ref()
            .map(|shm| shm.check(&self.root, budget, true))
            .transpose()?
            .unwrap_or(false);
        self.shape(budget, shm_present)?;
        if shm_present {
            self.root.check(budget)?;
            ensure!(
                self.shm
                    .as_ref()
                    .context("owned SQL SHM absent")?
                    .check(&self.root, budget, false)?,
                "owned SQL SHM disappeared before cleanup"
            );
            budget.check()?;
            let name = component(OsStr::new(SHM))?;
            // Only the positively matched, privately created scratch inode is removed.
            ensure!(
                unsafe { libc::unlinkat(self.root.file.as_raw_fd(), name.as_ptr(), 0) } == 0,
                "removing owned SQL SHM: {}",
                std::io::Error::last_os_error()
            );
        }
        self.root.file.sync_all()?;
        budget.check()?;
        self.finished = true;
        self.check_closed(budget)
    }
    fn check_closed(&self, budget: &Budget) -> Result<()> {
        ensure!(self.finished, "SQL custody analysis did not close");
        self.root.check(budget)?;
        self.database.check(&self.root, budget, true)?;
        if let Some(wal) = &self.wal {
            wal.check(&self.root, budget, true)?;
        }
        if let Some(shm) = &self.shm {
            ensure!(
                !shm.check(&self.root, budget, true)?,
                "SQL SHM survived final closure"
            );
        }
        self.shape(budget, false)?;
        budget.check()
    }
}

#[cfg(test)]
fn with_checkpoints<R>(
    input: Capture<'_>,
    out: &Path,
    budget: &mut Budget,
    created: impl FnOnce(&CreatedOutput) -> Result<()>,
    live: impl FnOnce(&rusqlite::Connection, &CreatedOutput, &Budget) -> Result<()>,
    visitor: impl for<'a> FnOnce(Materialized<'a>) -> Result<R>,
) -> Result<R> {
    run(input, out, budget, created, live, visitor)
}
#[cfg(test)]
mod tests;

mod owned;
pub use owned::{OwnedMaterialized, materialize_owned_controlled};
