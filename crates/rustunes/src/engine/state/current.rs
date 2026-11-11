//! Fresh read-only pre/poststate for a journaled sync step. All copied inputs
//! come from one checked USB provider; the writer rechecks exact hashes at Ready.
//!
//! A capture leaves a Current directory: the files it copied and the state
//! receipt binding them, which the `receipt` module encodes and names with
//! the layout of the directory. The saved Current reader
//! (`engine::recovery::saved`) loads every saved or retained one again.
use std::{
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    future::Future,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{DirBuilderExt, MetadataExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, timeout_at};

use crate::{
    engine::execution::device, engine::state::device_files, engine::state::journal::Identity,
    engine::verification::catalogue_capture,
};
use music_protocol::catalogue::identity::{self, LibraryIdentity};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::trace::private_write;
use music_protocol::protocol::transfer::{
    CaptureControl, Transfer, bounded_error, current_json_bytes, regular_size, write_current_json,
};
use receipt::{Form, StateReceipt};

#[cfg(test)]
pub(crate) mod fixtures;
pub(crate) mod receipt;

pub struct Current {
    pub device: String,
    pub identity: Identity,
    pub library: LibraryIdentity,
    pub catalog: Catalog,
    pub raw_preferences: Vec<u8>,
    pub plist_preferences: Vec<u8>,
    /// Always captured: present, or absent on a phone no Apple computer has
    /// synced (one `rustunes init` prepared).
    pub cdb: DeviceFile,
    /// Captured only for insert and removal batches and covers.
    pub artwork_db: DeviceFile,
    pub receipt_sha256: [u8; 32],
}

/// A file a computer's music library leaves on the phone (iTunesCDB,
/// ArtworkDB), as one capture saw it. Only Apple's computers write these, and
/// the phone never reads them (docs/10 §5), so a phone
/// `rustunes init` prepared never has them; rustunes then uses their defaults:
/// VoiceMemo anchor 1, the database PID from SQL, artwork cache IDs from 100.
#[derive(Clone, PartialEq, Eq)]
pub enum DeviceFile {
    /// The capture did not ask for it.
    NotCaptured,
    /// Asked for, and not on the phone.
    Absent,
    Present(Vec<u8>),
}

impl DeviceFile {
    /// The bytes, when the file is on the phone.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Present(bytes) => Some(bytes),
            _ => None,
        }
    }
    pub fn captured(&self) -> bool {
        !matches!(self, Self::NotCaptured)
    }
    /// The bytes of a captured file, or None when it is absent; an error when
    /// the capture did not ask for it.
    pub fn captured_bytes(&self, name: &str) -> Result<Option<&[u8]>> {
        ensure!(self.captured(), "{name} capture absent");
        Ok(self.bytes())
    }
    fn from_copy(bytes: Option<Vec<u8>>) -> Self {
        bytes.map_or(Self::Absent, Self::Present)
    }
}

/// A stricter enclosing deadline is retained across both complete attempts.
pub async fn capture_until(
    trace: &Path,
    expected: &Identity,
    deadline: Instant,
) -> Result<Current> {
    capture_options(trace, expected, false, deadline).await
}

pub async fn capture_controlled(
    trace: &Path,
    expected: &Identity,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Current> {
    capture_options_controlled(trace, expected, false, deadline, cancelled).await
}
pub async fn capture_with_artwork_controlled(
    trace: &Path,
    expected: &Identity,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Current> {
    capture_options_controlled(trace, expected, true, deadline, cancelled).await
}
async fn capture_options_controlled(
    trace: &Path,
    expected: &Identity,
    artwork: bool,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<Current> {
    let control = CaptureControl {
        deadline: deadline.min(Instant::now() + Duration::from_secs(120)),
        cancelled,
    };
    control.check()?;
    let result =
        capture_attempts_options(trace, control.deadline, Some(&control), |path, deadline| {
            let control = control.clone();
            async move {
                let current =
                    capture_inner_options(&path, expected, deadline, artwork, Some(&control))
                        .await?;
                let receipt = current.receipt_sha256;
                Ok((current, receipt))
            }
        })
        .await;
    // Original typed errors govern the private retry loop. Public diagnostics
    // never carry an unbounded transport/provider error body.
    finish_controlled(&control, result)
}
// The same original control fences the public successful return after the
// awaited attempt loop; the generic value provides no Current authority.
fn finish_controlled<T>(control: &CaptureControl, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            control.check()?;
            Ok(value)
        }
        Err(error) => Err(anyhow::anyhow!(bounded_error(&error))),
    }
}
fn check_capture(control: Option<&CaptureControl>) -> Result<()> {
    if let Some(control) = control {
        control.check()?;
    }
    Ok(())
}
fn write_capture_receipt(
    path: &Path,
    record: &impl serde::Serialize,
    control: Option<&CaptureControl>,
) -> Result<()> {
    check_capture(control)?;
    if control.is_some() {
        write_current_json(path, record)?;
    } else {
        private_write(path, &serde_json::to_vec_pretty(record)?)?;
    }
    check_capture(control)
}

async fn capture_options(
    trace: &Path,
    expected: &Identity,
    artwork: bool,
    deadline: Instant,
) -> Result<Current> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(120));
    capture_attempts(trace, deadline, |path, deadline| async move {
        let current = capture_inner(&path, expected, deadline, artwork).await?;
        let receipt = current.receipt_sha256;
        // Current/Catalog contain owned data and digests, never staging paths.
        Ok((current, receipt))
    })
    .await
}

/// This loop can only rerun the read-only complete-state closure. Native ATC,
/// upload, verification and journal transition code are outside this module.
async fn capture_attempts<T, F, Fut>(trace: &Path, deadline: Instant, capture: F) -> Result<T>
where
    F: FnMut(PathBuf, Instant) -> Fut,
    Fut: Future<Output = Result<(T, [u8; 32])>>,
{
    capture_attempts_options(trace, deadline, None, capture).await
}

async fn capture_attempts_options<T, F, Fut>(
    trace: &Path,
    deadline: Instant,
    control: Option<&CaptureControl>,
    mut capture: F,
) -> Result<T>
where
    F: FnMut(PathBuf, Instant) -> Fut,
    Fut: Future<Output = Result<(T, [u8; 32])>>,
{
    ensure!(
        Instant::now() < deadline,
        "fresh state capture deadline expired before discovery"
    );
    check_capture(control)?;
    let storage = AttemptStorage::create(trace)?;
    check_capture(control)?;
    let mut failures = Vec::new();
    for number in 1..=2 {
        ensure!(
            Instant::now() < deadline,
            "fresh state capture original deadline expired; failed attempts retained"
        );
        check_capture(control)?;
        storage.check()?;
        let attempt = storage.attempt(number)?;
        check_capture(control)?;
        let result = if let Some(control) = control {
            control
                .run(
                    Duration::from_secs(120),
                    "fresh controlled state original deadline",
                    capture(attempt.fd_path(), deadline),
                )
                .await
        } else {
            timeout_at(deadline, capture(attempt.fd_path(), deadline))
                .await
                .context("fresh state capture original deadline")
                .and_then(|result| result)
        };
        storage.check()?;
        attempt.check(&storage.directory)?;
        match result {
            Ok((value, receipt)) => {
                ensure!(
                    Instant::now() < deadline,
                    "fresh state capture original deadline expired before publication"
                );
                let record = serde_json::json!({"schema":"rustunes.capture-attempt", "attempt":number,
                    "status":"captured", "state_receipt_sha256":hex(&receipt), "prior_failed_attempts":failures,
                    "maximum_attempts":2, "deadline_renewed":false, "phone_writes":0});
                write_capture_receipt(
                    &attempt.fd_path().join("capture-attempt.json"),
                    &record,
                    control,
                )?;
                check_capture(control)?;
                attempt.sync()?;
                check_capture(control)?;
                ensure!(
                    Instant::now() < deadline,
                    "fresh state capture original deadline expired before publication"
                );
                check_capture(control)?;
                storage.publish(&attempt)?;
                check_capture(control)?;
                let record = serde_json::json!({"schema":"rustunes.capture-selection", "selected_attempt":number,
                    "canonical_trace":storage.trace, "state_receipt_sha256":hex(&receipt), "failed_attempts":failures,
                    "maximum_attempts":2, "deadline_renewed":false, "phone_writes":0});
                write_capture_receipt(&storage.fd_path().join("selection.json"), &record, control)?;
                check_capture(control)?;
                storage.directory.sync_all()?;
                check_capture(control)?;
                storage.check()?;
                same_directory(
                    &attempt.directory,
                    &open_dir_at(&storage.parent, &storage.target)?,
                )?;
                ensure!(
                    Instant::now() < deadline,
                    "fresh state capture original deadline expired during publication; capture retained but not returned"
                );
                crate::engine::observe(crate::engine::Event::Observation(
                    crate::engine::Observation::StateCaptureRetained(&storage.trace),
                ))?;
                check_capture(control)?;
                return Ok(value);
            }
            Err(error) => {
                let changed = error
                    .downcast_ref::<catalogue_capture::CatalogCaptureDrift>()
                    .map(|drift| drift.changed_files().to_vec());
                let dropped = music_protocol::protocol::afc::dropped_by_phone(&error);
                let message = if control.is_some() {
                    bounded_error(&error)
                } else {
                    format!("{error:#}")
                };
                let record = serde_json::json!({"schema":"rustunes.capture-attempt", "attempt":number,
                    "status":"failed", "error":message, "catalog_drift_files":changed,
                    "connection_dropped":dropped, "maximum_attempts":2, "deadline_renewed":false, "phone_writes":0});
                let diagnostic = format!("{message}\n");
                let error_path = attempt.fd_path().join("error.txt");
                private_write(&error_path, diagnostic.as_bytes())?;
                let receipt = if control.is_some() {
                    current_json_bytes(&record)?
                } else {
                    serde_json::to_vec_pretty(&record)?
                };
                let receipt_path = attempt.fd_path().join("capture-attempt.json");
                private_write(&receipt_path, &receipt)?;
                attempt.sync()?;
                storage.directory.sync_all()?;
                failures.push(serde_json::json!({"directory":attempt.name.to_string_lossy(), "receipt_sha256":hex(&Sha256::digest(&receipt)), "catalog_drift":changed.is_some(), "connection_dropped":dropped}));
                if (changed.is_none() && !dropped) || number == 2 {
                    return Err(error.context(format!(
                        "complete state capture failed; evidence retained at {}",
                        storage.path.display()
                    )));
                }
                crate::engine::observe(crate::engine::Event::Observation(if changed.is_some() {
                    crate::engine::Observation::RecapturingState
                } else {
                    crate::engine::Observation::RecapturingAfterDroppedConnection
                }))?;
                if let Some(control) = control {
                    control
                        .run(
                            Duration::from_secs(120),
                            "controlled capture retry delay",
                            async {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                Ok(())
                            },
                        )
                        .await?;
                } else {
                    timeout_at(deadline, tokio::time::sleep(Duration::from_millis(250)))
                        .await
                        .context(
                            "fresh state capture original deadline expired before second attempt",
                        )?;
                }
            }
        }
    }
    unreachable!("two-attempt loop always returns")
}

/// Hold directory descriptors and access owned attempts through /proc/self/fd,
/// so an ancestor rename cannot redirect capture writes. Only Linux's atomic
/// no-replace publication is supported; no clobbering fallback is permitted.
struct AttemptStorage {
    trace: PathBuf,
    parent_path: PathBuf,
    parent: File,
    target: OsString,
    path: PathBuf,
    name: OsString,
    directory: File,
}
impl AttemptStorage {
    fn create(trace: &Path) -> Result<Self> {
        let trace = std::path::absolute(trace)?;
        ensure!(
            trace.to_str().is_some(),
            "capture trace path must be UTF-8 for its evidence receipt"
        );
        let target = trace
            .file_name()
            .context("capture trace must name a new directory")?
            .to_owned();
        let parent_path = trace.parent().context("capture parent missing")?.to_owned();
        let parent = open_parent(&parent_path)?;
        ensure_absent(&parent, &target)?;
        let mut name = target.clone();
        name.push(".capture-attempts");
        make_dir_at(&parent, &name).context("capture attempt directory must be new")?;
        let directory = open_dir_at(&parent, &name)?;
        parent.sync_all()?;
        let path = parent_path.join(&name);
        let value = Self {
            trace,
            parent_path,
            parent,
            target,
            path,
            name,
            directory,
        };
        value.check()?;
        Ok(value)
    }
    fn fd_path(&self) -> PathBuf {
        fd_path(&self.directory)
    }
    fn check(&self) -> Result<()> {
        same_directory(&self.parent, &open_parent(&self.parent_path)?)?;
        same_directory(&self.directory, &open_dir_at(&self.parent, &self.name)?)
    }
    fn attempt(&self, number: u8) -> Result<AttemptDirectory> {
        let name = OsString::from(format!("attempt-{number:04}"));
        make_dir_at(&self.directory, &name)?;
        let directory = open_dir_at(&self.directory, &name)?;
        self.directory.sync_all()?;
        Ok(AttemptDirectory { directory, name })
    }
    fn publish(&self, attempt: &AttemptDirectory) -> Result<()> {
        self.check()?;
        attempt.check(&self.directory)?;
        let from = component(&attempt.name)?;
        let to = component(&self.target)?;
        // SAFETY: live owned descriptors, NUL-terminated single components and
        // Linux RENAME_NOREPLACE prevent replacing any existing target entry.
        let result = unsafe {
            libc::renameat2(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.parent.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("publish capture without replacing existing target; evidence retained");
        }
        self.directory.sync_all()?;
        self.parent.sync_all()?;
        self.check()?;
        same_directory(
            &attempt.directory,
            &open_dir_at(&self.parent, &self.target)?,
        )
    }
}
struct AttemptDirectory {
    directory: File,
    name: OsString,
}
impl AttemptDirectory {
    fn fd_path(&self) -> PathBuf {
        fd_path(&self.directory)
    }
    fn check(&self, parent: &File) -> Result<()> {
        same_directory(&self.directory, &open_dir_at(parent, &self.name)?)
    }
    fn sync(&self) -> Result<()> {
        // Every file is already synced by private_write/capture_client. Sync
        // their containing directories before publishing the complete tree.
        for name in ["afc", "catalog"] {
            match open_dir_at(&self.directory, OsStr::new(name)) {
                Ok(directory) => {
                    if name == "catalog" {
                        // Failed/cancelled copies may have only partial bytes.
                        // Sync these too; completed copies and all receipts are
                        // already synced by their writers.
                        for file in ["MediaLibrary.sqlitedb", "MediaLibrary.sqlitedb-wal"] {
                            match open_file_at(&directory, OsStr::new(file)) {
                                Ok(file) => {
                                    ensure!(
                                        file.metadata()?.is_file(),
                                        "catalog copy is not a regular file"
                                    );
                                    file.sync_all()?;
                                }
                                Err(error) if is_not_found(&error) => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    directory.sync_all()?;
                }
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
        self.directory.sync_all()?;
        Ok(())
    }
}
fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
fn component(name: &OsStr) -> Result<CString> {
    ensure!(
        !name.is_empty() && name != "." && name != ".." && !name.as_bytes().contains(&b'/'),
        "invalid capture directory component"
    );
    Ok(CString::new(name.as_bytes())?)
}
fn open_dir_at(parent: &File, name: &OsStr) -> Result<File> {
    open_at(parent, name, libc::O_DIRECTORY)
}
fn open_file_at(parent: &File, name: &OsStr) -> Result<File> {
    open_at(parent, name, 0)
}
fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
fn open_at(parent: &File, name: &OsStr, additional_flags: i32) -> Result<File> {
    let name = component(name)?;
    // SAFETY: valid parent descriptor and C string; returned descriptor is
    // checked and transferred into one File owner exactly once.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | additional_flags,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn make_dir_at(parent: &File, name: &OsStr) -> Result<()> {
    let name = component(name)?;
    // SAFETY: live directory descriptor and NUL-terminated single component.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn ensure_absent(parent: &File, name: &OsStr) -> Result<()> {
    component(name)?;
    match fs::symlink_metadata(fd_path(parent).join(name)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => anyhow::bail!("capture trace already exists"),
        Err(error) => Err(error.into()),
    }
}
fn open_parent(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "capture parent must be absolute");
    let mut directory = File::open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => directory = open_dir_at(&directory, name)?,
            _ => anyhow::bail!("capture parent cannot contain parent traversal"),
        }
    }
    Ok(directory)
}
fn same_directory(held: &File, current: &File) -> Result<()> {
    let a = held.metadata()?;
    let b = current.metadata()?;
    ensure!(
        a.is_dir() && b.is_dir() && a.dev() == b.dev() && a.ino() == b.ino(),
        "capture directory identity changed"
    );
    Ok(())
}

async fn capture_inner(
    trace: &Path,
    expected: &Identity,
    deadline: Instant,
    include_artwork: bool,
) -> Result<Current> {
    capture_inner_options(trace, expected, deadline, include_artwork, None).await
}

async fn capture_inner_options(
    trace: &Path,
    expected: &Identity,
    deadline: Instant,
    include_artwork: bool,
    control: Option<&CaptureControl>,
) -> Result<Current> {
    ensure!(
        Instant::now() < deadline,
        "fresh state capture deadline expired before discovery"
    );
    check_capture(control)?;
    let provider = if let Some(control) = control {
        control
            .run(
                Duration::from_secs(15),
                "state USB discovery deadline",
                device::provider(),
            )
            .await?
    } else {
        timeout_at(
            deadline.min(Instant::now() + Duration::from_secs(15)),
            device::provider(),
        )
        .await
        .context("state USB discovery deadline")??
    };
    let device = provider.udid.clone();
    let transfer_path = trace.join("afc");
    check_capture(control)?;
    fs::DirBuilder::new().mode(0o700).create(&transfer_path)?;
    check_capture(control)?;
    let mut afc = if let Some(control) = control {
        Transfer::connect_current_summary(&provider, &transfer_path, control).await?
    } else {
        Transfer::connect(&provider, &transfer_path, deadline).await?
    };
    let mut names = vec![
        device_files::PREFS,
        device_files::PREFS_PLIST,
        device_files::CDB,
    ];
    let mut caps = vec![
        receipt::PREFS.cap,
        receipt::PREFS_PLIST.cap,
        receipt::CDB.cap,
    ];
    if include_artwork {
        names.push(device_files::ARTWORK_DB);
        caps.push(receipt::ARTWORK_DB.cap);
    }
    // The prefs pair is required; Apple's CDB and ArtworkDB may be absent.
    let optional = |name: &str| [device_files::CDB, device_files::ARTWORK_DB].contains(&name);
    let mut attrs = Vec::new();
    let mut copies = Vec::new();
    for (index, name) in names.iter().enumerate() {
        check_capture(control)?;
        let Some(observed) = afc.stat(name).await? else {
            if optional(name) {
                attrs.push(None);
                copies.push(None);
                continue;
            }
            let mut folder = None;
            for parent in device_files::folders(name) {
                if afc.stat(parent).await?.is_none() {
                    folder = Some(parent.to_owned());
                    break;
                }
            }
            let other_half = match device_files::other_half(name) {
                Some(other) if folder.is_none() => afc.stat(other).await?.is_some(),
                _ => false,
            };
            return Err(device_files::MissingDeviceFile::new(name, folder, other_half).into());
        };
        let size: u64 = observed
            .get("st_size")
            .context("device file size missing")?
            .parse()?;
        regular_size(&observed, size)?;
        ensure!(
            size > 0 && size <= caps[index],
            "device library input exceeds bound"
        );
        let mut bytes = Vec::new();
        afc.read_exact(name, size, caps[index], &mut bytes).await?;
        check_capture(control)?;
        let path = trace.join(name.rsplit('/').next().unwrap());
        private_write(&path, &bytes)?;
        check_capture(control)?;
        attrs.push(Some(observed));
        copies.push(Some(bytes));
    }
    check_capture(control)?;
    let (Some(raw), Some(plist)) = (&copies[0], &copies[1]) else {
        anyhow::bail!("prefs copies missing");
    };
    let library = identity::parse(raw)?;
    library.ensure_matches(&identity::parse(plist)?)?;
    let identity = Identity::new(&device, &library)?;
    ensure!(
        identity.device_digest() == expected.device_digest()
            && identity.library_digest() == expected.library_digest(),
        "fresh device or library differs from saved plan"
    );
    let catalog = if let Some(control) = control {
        catalogue_capture::read_until_controlled(&trace.join("catalog"), control, || async {
            Ok(provider)
        })
        .await?
    } else {
        catalogue_capture::read_within(&trace.join("catalog"), deadline, || async { Ok(provider) })
            .await?
    };
    for (index, name) in names.iter().enumerate() {
        let (Some(before), Some(copy)) = (&attrs[index], &copies[index]) else {
            ensure!(
                afc.stat(name).await?.is_none(),
                "{name} appeared on the phone during capture"
            );
            continue;
        };
        let mut after = afc
            .stat(name)
            .await?
            .context("device library input disappeared")?;
        let mut before = before.clone();
        before.remove("st_atime");
        after.remove("st_atime");
        ensure!(
            after == before,
            "device library input attributes changed during capture"
        );
        ensure!(
            afc.read_hash(name, copy.len() as u64, caps[index]).await?
                == <[u8; 32]>::from(Sha256::digest(copy)),
            "device library input bytes changed during capture"
        );
    }
    drop(afc);
    let artwork_db = if include_artwork {
        DeviceFile::from_copy(copies.pop().unwrap())
    } else {
        DeviceFile::NotCaptured
    };
    let cdb = DeviceFile::from_copy(copies.pop().unwrap());
    let plist_preferences = copies.pop().unwrap().unwrap();
    let raw_preferences = copies.pop().unwrap().unwrap();
    check_capture(control)?;
    let anchors = cdb
        .bytes()
        .map(music_protocol::catalogue::anchor::parse)
        .transpose()?;
    check_capture(control)?;
    if let Some(anchors) = &anchors {
        ensure!(
            anchors.database_pid() == catalog.database_pid(),
            "copied CDB and SQL catalogue identities differ"
        );
    }
    let receipt = StateReceipt::describe(
        &identity,
        &catalog,
        &raw_preferences,
        &plist_preferences,
        &cdb,
        &artwork_db,
        anchors.as_ref(),
    );
    check_capture(control)?;
    let bytes = receipt.encode(if control.is_some() {
        Form::Capped
    } else {
        Form::Pretty
    })?;
    check_capture(control)?;
    private_write(&receipt::RECEIPT.at(trace), &bytes)?;
    check_capture(control)?;
    File::open(trace)?.sync_all()?;
    check_capture(control)?;
    Ok(Current {
        device,
        identity,
        library,
        catalog,
        raw_preferences,
        plist_preferences,
        cdb,
        artwork_db,
        receipt_sha256: Sha256::digest(&bytes).into(),
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
