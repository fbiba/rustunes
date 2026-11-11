//! Bounded saved-evidence configuration and Current loader. No endpoint access.
use super::io;
use crate::{
    engine::state::current::{
        Current, DeviceFile,
        receipt::{self, CurrentFile, Recorded, StateReceipt},
    },
    engine::state::journal::Identity,
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::anchor;
use music_protocol::catalogue::sqlite;
use music_protocol::media::artwork_inventory;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedCurrent {
    pub directory: PathBuf,
    pub receipt_sha256: String,
}
impl SavedCurrent {
    fn validate(&self) -> Result<()> {
        path(&self.directory)?;
        io::digest(&self.receipt_sha256)?;
        Ok(())
    }
}

fn path(p: &Path) -> Result<()> {
    let text = p.to_str().context("non-UTF8 evidence path")?;
    ensure!(
        p.is_absolute()
            && text.len() <= 4096
            && !text.chars().any(char::is_control)
            && p.components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "evidence path must be bounded absolute and contain no traversal"
    );
    Ok(())
}
pub(super) fn exists(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(crate) struct LoadedCurrent {
    pub current: Current,
    pub receipt: Vec<u8>,
}
/// Saved observations only: no Clone/serde or public constructor/conversion to
/// Current.
struct LoadedCurrentFacts {
    device: String,
    identity: Identity,
    library: music_protocol::catalogue::identity::LibraryIdentity,
    catalog: sqlite::Catalog,
    raw_preferences: Vec<u8>,
    plist_preferences: Vec<u8>,
    cdb: DeviceFile,
    artwork_db: DeviceFile,
    receipt: Vec<u8>,
}
impl LoadedCurrentFacts {
    fn into_loaded_current(self) -> LoadedCurrent {
        LoadedCurrent {
            current: Current {
                device: self.device,
                identity: self.identity,
                library: self.library,
                catalog: self.catalog,
                raw_preferences: self.raw_preferences,
                plist_preferences: self.plist_preferences,
                cdb: self.cdb,
                artwork_db: self.artwork_db,
                receipt_sha256: io::sha(&self.receipt),
            },
            receipt: self.receipt,
        }
    }
}
/// Load the exact saved receipt and its pinned files. The caller-selected device
/// is checked against receipt hashes, journal scope and copied raw+plist prefs.
pub(crate) fn load_current(
    spec: &SavedCurrent,
    device: &str,
    scratch: &Path,
) -> Result<LoadedCurrent> {
    spec.validate()?;
    let pin = io::digest(&spec.receipt_sha256)?;
    load_current_checked(&spec.directory, Some(pin), device, scratch, None)
}
pub(crate) fn load_current_with_control(
    spec: &SavedCurrent,
    device: &str,
    scratch: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<LoadedCurrent> {
    spec.validate()?;
    let pin = io::digest(&spec.receipt_sha256)?;
    let control = Some((deadline, cancelled));
    load_current_checked(&spec.directory, Some(pin), device, scratch, control)
}
/// Load the Current at `directory` whatever receipt it holds, for evidence
/// nothing earlier pins (a step's immediate After). Every file must still
/// be the one that receipt binds.
pub(crate) fn load_unpinned(
    directory: &Path,
    device: &str,
    scratch: &Path,
) -> Result<LoadedCurrent> {
    load_current_checked(directory, None, device, scratch, None)
}
pub(crate) fn load_unpinned_with_control(
    directory: &Path,
    device: &str,
    scratch: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<LoadedCurrent> {
    load_current_checked(
        directory,
        None,
        device,
        scratch,
        Some((deadline, cancelled)),
    )
}
fn check_current_control(control: Option<&(Instant, Arc<AtomicBool>)>) -> Result<()> {
    if let Some((deadline, cancelled)) = control {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "saved Current read cancelled"
        );
        ensure!(
            Instant::now() < *deadline,
            "saved Current read deadline exceeded"
        );
    }
    Ok(())
}
fn load_current_checked(
    directory: &Path,
    pin: Option<[u8; 32]>,
    device: &str,
    scratch: &Path,
    controlled: Option<(Instant, Arc<AtomicBool>)>,
) -> Result<LoadedCurrent> {
    let loaded = load_current_facts_checked(directory, pin, device, scratch, controlled.clone())?
        .into_loaded_current();
    check_current_control(controlled.as_ref())?;
    Ok(loaded)
}
fn load_current_facts_checked(
    directory: &Path,
    pin: Option<[u8; 32]>,
    device: &str,
    scratch: &Path,
    controlled: Option<(Instant, Arc<AtomicBool>)>,
) -> Result<LoadedCurrentFacts> {
    let control = controlled.as_ref();
    check_current_control(control)?;
    let read = |file: CurrentFile| {
        check_current_control(control)?;
        let bytes = io::read(&file.at(directory), file.cap)?;
        check_current_control(control)?;
        Ok::<_, anyhow::Error>(bytes)
    };
    let hash = |file: CurrentFile| match control {
        None => io::file_hash(&file.at(directory), file.cap),
        Some((deadline, cancelled)) => {
            io::file_hash_with_control(&file.at(directory), file.cap, *deadline, cancelled.clone())
        }
    };
    // A file the receipt binds: read whole, its hash the one recorded.
    let bound = |file: CurrentFile, sha256: [u8; 32]| {
        let bytes = read(file)?;
        check_current_control(control)?;
        ensure!(
            io::sha(&bytes) == sha256,
            "saved Current {} differs from receipt",
            file.path
        );
        Ok::<_, anyhow::Error>(bytes)
    };
    // A file the phone may not have had. With no hash recorded, nothing may
    // lie at its path.
    let copied = |file: CurrentFile, recorded: Recorded| {
        let unbound = || {
            ensure!(
                !exists(&file.at(directory))?,
                "saved Current contains an unbound {}",
                file.path
            );
            Ok::<_, anyhow::Error>(())
        };
        Ok::<_, anyhow::Error>(match recorded {
            Recorded::Hash(sha256) => DeviceFile::Present(bound(file, sha256)?),
            Recorded::Absent => {
                unbound()?;
                DeviceFile::Absent
            }
            Recorded::NotCaptured => {
                unbound()?;
                DeviceFile::NotCaptured
            }
        })
    };
    path(directory)?;
    io::directory(directory)?;
    let bytes = read(receipt::RECEIPT)?;
    if let Some(pin) = pin {
        ensure!(io::sha(&bytes) == pin, "saved Current receipt differs");
    }
    let receipt = StateReceipt::decode(&bytes).context("unsupported Current receipt")?;
    let raw_preferences = bound(receipt::PREFS, receipt.raw_preferences_sha256())?;
    let plist_preferences = bound(receipt::PREFS_PLIST, receipt.plist_preferences_sha256())?;
    // No CDB: the phone had none (one `rustunes init` prepared).
    let cdb = copied(receipt::CDB, receipt.cdb())?;
    check_current_control(control)?;
    let library = music_protocol::catalogue::identity::parse(&raw_preferences)?;
    library.ensure_matches(&music_protocol::catalogue::identity::parse(
        &plist_preferences,
    )?)?;
    let identity = Identity::new(device, &library)?;
    ensure!(
        receipt.describes_identity(&identity),
        "saved Current device/library differs"
    );
    let artwork_db = copied(receipt::ARTWORK_DB, receipt.artwork_db())?;
    if let Some(bytes) = artwork_db.bytes() {
        artwork_inventory::parse(bytes)?;
        check_current_control(control)?;
    }
    let database = receipt::DATABASE.at(directory);
    let wal = receipt::WAL.at(directory);
    let has_wal = receipt.wal_sha256().is_some();
    ensure!(
        exists(&wal)? == has_wal,
        "saved Current WAL presence differs"
    );
    let (_, database_sha) = hash(receipt::DATABASE)?;
    ensure!(
        database_sha == receipt.database_sha256(),
        "saved Current database differs"
    );
    if let Some(wal_sha) = receipt.wal_sha256() {
        ensure!(
            hash(receipt::WAL)?.1 == wal_sha,
            "saved Current WAL differs"
        );
    }
    let catalog = match control {
        None => sqlite::read_snapshot(&database, has_wal.then_some(wal.as_path()), scratch)?,
        Some((deadline, cancelled)) => sqlite::read_snapshot_with_control(
            &database,
            has_wal.then_some(wal.as_path()),
            scratch,
            *deadline,
            Arc::clone(cancelled),
        )?,
    };
    check_current_control(control)?;
    ensure!(
        receipt.describes_catalog(&catalog),
        "saved Current SQL binding differs"
    );
    // The receipt records saved anchors exactly when it binds a CDB.
    if let Some(cdb) = cdb.bytes() {
        let anchor = anchor::parse(cdb)?;
        ensure!(
            anchor.database_pid() == catalog.database_pid()
                && receipt.describes_saved_anchors(Some(&anchor)),
            "saved CDB binding differs"
        );
    }
    check_current_control(control)?;
    let loaded = LoadedCurrentFacts {
        device: device.into(),
        identity,
        library,
        catalog,
        raw_preferences,
        plist_preferences,
        cdb,
        artwork_db,
        receipt: bytes,
    };
    check_current_control(control)?;
    Ok(loaded)
}
