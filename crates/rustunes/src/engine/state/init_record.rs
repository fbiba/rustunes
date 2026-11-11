//! The library rustunes created for a phone no computer had synced
//! (`rustunes init`): the two identity components it wrote to the phone's
//! prefs, and the database and library-playlist PIDs of its first session.
//!
//! Kept per phone in `devices/<device digest>/init.json`, private, created
//! once and never changed, so a dry run, the real run and a retry after an
//! interrupted first session all use the same values. Beside it, the operation
//! pair the last attempt staged, so a retry can remove exactly that pair,
//! and `init-run`, naming the run directory of the last attempt that went
//! to the phone, which pruning keeps whole: init's messages point to it.
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::prefs::{self, Prefs};
use serde_json::{Value, json};

use crate::engine::state::layout::{self, private_directory};

const SCHEMA: &str = "rustunes.init";
const RECORD: &str = "init.json";
const STAGED: &str = "init-staged.plist";
const STAGED_SIGNATURE: &str = "init-staged.plist.cig";
const RUN: &str = "init-run";
const MAX_BYTES: u64 = 64 * 1024;

/// No Debug: it holds the library identity components.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct InitRecord {
    pub catalog_id: u64,
    pub user_id: u64,
    pub database_pid: u64,
    pub library_playlist_pid: u64,
}

impl InitRecord {
    /// New random values: every one nonzero, the identity's XOR nonzero, the
    /// two PIDs distinct.
    pub fn mint() -> Result<Self> {
        let mut random = File::open("/dev/urandom")?;
        loop {
            let mut bytes = [0; 32];
            random.read_exact(&mut bytes)?;
            let value = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            let record = Self {
                catalog_id: value(0),
                user_id: value(8),
                database_pid: value(16),
                library_playlist_pid: value(24),
            };
            if record.valid() {
                return Ok(record);
            }
        }
    }

    fn valid(&self) -> bool {
        self.catalog_id != 0
            && self.user_id != 0
            && self.catalog_id ^ self.user_id != 0
            && self.database_pid != 0
            && self.library_playlist_pid != 0
            && self.database_pid != self.library_playlist_pid
    }

    /// The prefs pair this record writes to the phone.
    pub fn prefs(&self, host_name: &str) -> Result<Prefs> {
        prefs::adoption(self.catalog_id, self.user_id, host_name)
    }

    /// The pair this record wrote, under whatever computer name, whose
    /// `iTunesPrefs` is `bytes` or begins with them (what an interrupted write
    /// of it left). None when this record wrote no such pair.
    pub fn written_prefs(&self, bytes: &[u8]) -> Option<Prefs> {
        prefs::adoption_starting(bytes, self.catalog_id, self.user_id)
    }

    /// Whether `prefs` carry this record's identity.
    pub fn owns(
        &self,
        prefs: &music_protocol::catalogue::identity::LibraryIdentity,
    ) -> Result<bool> {
        Ok(self.prefs("rustunes")?.identity()?.binding_sha256() == prefs.binding_sha256())
    }

    fn to_json(&self) -> Value {
        json!({
            "schema": SCHEMA,
            "catalog_id": format!("{:016X}", self.catalog_id),
            "user_id": format!("{:016X}", self.user_id),
            "database_pid": format!("{:016X}", self.database_pid),
            "library_playlist_pid": format!("{:016X}", self.library_playlist_pid),
        })
    }

    fn from_json(value: &Value) -> Result<Self> {
        ensure!(value["schema"] == SCHEMA, "init record schema differs");
        let field = |name: &str| -> Result<u64> {
            let text = value[name]
                .as_str()
                .with_context(|| format!("init record {name} is missing"))?;
            ensure!(
                text.len() == 16
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase()),
                "init record {name} is not 16 uppercase hex digits"
            );
            Ok(u64::from_str_radix(text, 16)?)
        };
        let record = Self {
            catalog_id: field("catalog_id")?,
            user_id: field("user_id")?,
            database_pid: field("database_pid")?,
            library_playlist_pid: field("library_playlist_pid")?,
        };
        ensure!(record.valid(), "init record values are invalid");
        Ok(record)
    }
}

/// The phone's directory under `state`: `devices/<device digest hex>`.
pub(crate) fn directory(state: &Path, device: &[u8; 32]) -> PathBuf {
    state
        .join("devices")
        .join(crate::engine::state::current::hex(device))
}

/// The phone's record, if rustunes initialised it from this state directory.
pub(crate) fn load(state: &Path, device: &[u8; 32]) -> Result<Option<InitRecord>> {
    let path = directory(state, device).join(RECORD);
    let Some(bytes) = read_private(&path)? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&bytes).context("init record is not JSON")?;
    InitRecord::from_json(&value).map(Some)
}

/// The phone's record, created with new values when there is none.
pub(crate) fn load_or_create(state: &Path, device: &[u8; 32]) -> Result<InitRecord> {
    if let Some(record) = load(state, device)? {
        return Ok(record);
    }
    let directory = directory(state, device);
    private_directory(&directory)?;
    let record = InitRecord::mint()?;
    write_new(
        &directory.join(RECORD),
        &serde_json::to_vec_pretty(&record.to_json())?,
    )?;
    Ok(record)
}

/// Keep the operation pair an attempt is about to stage, replacing the last
/// attempt's, before anything is staged.
pub(crate) fn save_staged(
    state: &Path,
    device: &[u8; 32],
    operation: &[u8],
    signature: &[u8; 21],
) -> Result<()> {
    let directory = directory(state, device);
    private_directory(&directory)?;
    for (name, bytes) in [(STAGED, operation), (STAGED_SIGNATURE, &signature[..])] {
        let temporary = directory.join(format!("{name}.new"));
        let _ = fs::remove_file(&temporary);
        write_new(&temporary, bytes)?;
        fs::rename(&temporary, directory.join(name))?;
    }
    File::open(&directory)?.sync_all()?;
    Ok(())
}

/// Name `run` as the phone's latest init run, replacing the last attempt's,
/// before the attempt connects to the phone.
pub(crate) fn save_run(state: &Path, device: &[u8; 32], run: &Path) -> Result<()> {
    let name = run
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| layout::is_run_name(name))
        .with_context(|| format!("{} is not a run directory", run.display()))?;
    let directory = directory(state, device);
    private_directory(&directory)?;
    let temporary = directory.join(format!("{RUN}.new"));
    let _ = fs::remove_file(&temporary);
    write_new(&temporary, name.as_bytes())?;
    fs::rename(&temporary, directory.join(RUN))?;
    File::open(&directory)?.sync_all()?;
    Ok(())
}

/// The run directory name a phone's directory (`devices/<device digest>`)
/// names as its latest init run, if any.
pub(crate) fn latest_run(device_directory: &Path) -> Result<Option<OsString>> {
    let path = device_directory.join(RUN);
    let Some(bytes) = read_private(&path)? else {
        return Ok(None);
    };
    let name = String::from_utf8(bytes)
        .ok()
        .filter(|name| layout::is_run_name(name))
        .with_context(|| format!("{} does not name a run directory", path.display()))?;
    Ok(Some(name.into()))
}

/// The operation pair the last attempt staged, if one was saved.
pub(crate) fn staged(state: &Path, device: &[u8; 32]) -> Result<Option<(Vec<u8>, [u8; 21])>> {
    let directory = directory(state, device);
    let (Some(operation), Some(signature)) = (
        read_private(&directory.join(STAGED))?,
        read_private(&directory.join(STAGED_SIGNATURE))?,
    ) else {
        return Ok(None);
    };
    let signature: [u8; 21] = signature
        .try_into()
        .ok()
        .context("saved staged signature is not 21 bytes")?;
    Ok(Some((operation, signature)))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("record path has no parent")?)?.sync_all()?;
    Ok(())
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file()
            && metadata.len() <= MAX_BYTES
            && metadata.permissions().mode() & 0o077 == 0,
        "{} must be a private regular file",
        path.display()
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    fn state() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rustunes-init-record-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        path
    }

    #[test]
    fn a_record_is_created_once_and_read_back_unchanged() {
        let state = state();
        let device = [7; 32];
        assert!(load(&state, &device).unwrap().is_none());
        let first = load_or_create(&state, &device).unwrap();
        assert!(first.valid());
        let again = load_or_create(&state, &device).unwrap();
        assert!(first == again);
        assert!(load(&state, &device).unwrap().unwrap() == first);
        let file = directory(&state, &device).join(RECORD);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let prefs = first.prefs("rustunes").unwrap();
        assert!(first.owns(&prefs.identity().unwrap()).unwrap());
        let other = InitRecord::mint().unwrap();
        assert!(!other.owns(&prefs.identity().unwrap()).unwrap());
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn the_staged_pair_is_replaced_by_each_attempt() {
        let state = state();
        let device = [8; 32];
        assert!(staged(&state, &device).unwrap().is_none());
        save_staged(&state, &device, b"first", &[1; 21]).unwrap();
        save_staged(&state, &device, b"second", &[2; 21]).unwrap();
        let (operation, signature) = staged(&state, &device).unwrap().unwrap();
        assert_eq!(operation, b"second");
        assert_eq!(signature, [2; 21]);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn the_latest_init_run_is_replaced_by_each_attempt() {
        let state = state();
        let device = [6; 32];
        let directory = directory(&state, &device);
        assert!(latest_run(&directory).unwrap().is_none());
        let runs = state.join("runs");
        save_run(&state, &device, &runs.join("1-2")).unwrap();
        save_run(&state, &device, &runs.join("3-4")).unwrap();
        assert_eq!(latest_run(&directory).unwrap(), Some("3-4".into()));
        assert!(save_run(&state, &device, &runs.join("plan.json")).is_err());
        // A name that is not a run directory's is refused on reading too.
        for bad in [&b"../3-4"[..], b"3-4\n", b""] {
            fs::write(directory.join(RUN), bad).unwrap();
            assert!(latest_run(&directory).is_err());
        }
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn invalid_or_shared_records_are_refused() {
        let state = state();
        let device = [9; 32];
        let directory = directory(&state, &device);
        private_directory(&directory).unwrap();
        let file = directory.join(RECORD);
        let mut value = InitRecord::mint().unwrap().to_json();
        value["user_id"] = value["catalog_id"].clone();
        write_new(&file, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load(&state, &device).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&state, &device).is_err());
        fs::remove_dir_all(state).unwrap();
    }
}
