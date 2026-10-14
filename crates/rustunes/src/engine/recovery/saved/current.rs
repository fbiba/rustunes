//! Bounded saved-evidence configuration and Current loader. No endpoint access.
use super::io;
use crate::{engine::state::current::Current, engine::state::journal::Identity};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::anchor;
use music_protocol::catalogue::sqlite;
use music_protocol::media::artwork_inventory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
const SMALL: u64 = 16 * 1024 * 1024;

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
    cdb: Vec<u8>,
    artwork_db: Option<Vec<u8>>,
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
    load_current_checked(spec, device, scratch, None)
}
pub(crate) fn load_current_with_control(
    spec: &SavedCurrent,
    device: &str,
    scratch: &Path,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<LoadedCurrent> {
    load_current_checked(spec, device, scratch, Some((deadline, cancelled)))
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
    spec: &SavedCurrent,
    device: &str,
    scratch: &Path,
    controlled: Option<(Instant, Arc<AtomicBool>)>,
) -> Result<LoadedCurrent> {
    let loaded = load_current_facts_checked(spec, device, scratch, controlled.clone())?
        .into_loaded_current();
    check_current_control(controlled.as_ref())?;
    Ok(loaded)
}
fn load_current_facts_checked(
    spec: &SavedCurrent,
    device: &str,
    scratch: &Path,
    controlled: Option<(Instant, Arc<AtomicBool>)>,
) -> Result<LoadedCurrentFacts> {
    let control = controlled.as_ref();
    check_current_control(control)?;
    let read = |path: &Path, cap| {
        check_current_control(control)?;
        let bytes = io::read(path, cap)?;
        check_current_control(control)?;
        Ok::<_, anyhow::Error>(bytes)
    };
    let hash = |path: &Path, cap| match control {
        None => io::file_hash(path, cap),
        Some((deadline, cancelled)) => {
            io::file_hash_with_control(path, cap, *deadline, cancelled.clone())
        }
    };
    spec.validate()?;
    io::directory(&spec.directory)?;
    let receipt = read(&spec.directory.join("state-receipt.json"), SMALL)?;
    ensure!(
        io::sha(&receipt) == io::digest(&spec.receipt_sha256)?,
        "saved Current receipt differs"
    );
    let v: Value = serde_json::from_slice(&receipt)?;
    ensure!(
        v["schema"] == "rustunes.current-state"
            && v["phone_writes"] == 0
            && v["atomic_snapshot_claimed"] == false,
        "unsupported Current receipt"
    );
    let raw_preferences = read(&spec.directory.join("iTunesPrefs"), 1024 * 1024)?;
    let plist_preferences = read(&spec.directory.join("iTunesPrefs.plist"), 1024 * 1024)?;
    let cdb = read(&spec.directory.join("iTunesCDB"), 64 * 1024 * 1024)?;
    for (key, bytes) in [
        ("raw_preferences_sha256", &raw_preferences),
        ("plist_preferences_sha256", &plist_preferences),
        ("cdb_sha256", &cdb),
    ] {
        check_current_control(control)?;
        ensure!(
            io::json_hash(&v, key)? == io::sha(bytes),
            "saved Current file differs from receipt"
        );
    }
    check_current_control(control)?;
    let library = music_protocol::catalogue::identity::parse(&raw_preferences)?;
    library.ensure_matches(&music_protocol::catalogue::identity::parse(
        &plist_preferences,
    )?)?;
    let identity = Identity::new(device, &library)?;
    ensure!(
        io::json_hash(&v, "device_sha256")? == identity.device_digest()
            && io::json_hash(&v, "library_sha256")? == identity.library_digest(),
        "saved Current device/library differs"
    );
    let artwork_db = if v["artwork_database_sha256"].is_null() {
        ensure!(
            !exists(&spec.directory.join("ArtworkDB"))?,
            "ordinary Current contains unbound ArtworkDB"
        );
        None
    } else {
        let bytes = read(&spec.directory.join("ArtworkDB"), 32 * 1024 * 1024)?;
        ensure!(
            io::json_hash(&v, "artwork_database_sha256")? == io::sha(&bytes),
            "Current ArtworkDB differs"
        );
        artwork_inventory::parse(&bytes)?;
        check_current_control(control)?;
        Some(bytes)
    };
    let database = spec.directory.join("catalog/MediaLibrary.sqlitedb");
    let wal = spec.directory.join("catalog/MediaLibrary.sqlitedb-wal");
    let has_wal = !v["wal_sha256"].is_null();
    ensure!(
        exists(&wal)? == has_wal,
        "saved Current WAL presence differs"
    );
    let (_, database_sha) = hash(&database, sqlite::MAX_DATABASE_BYTES)?;
    ensure!(
        database_sha == io::json_hash(&v, "database_sha256")?,
        "saved Current database differs"
    );
    if has_wal {
        ensure!(
            hash(&wal, sqlite::MAX_WAL_BYTES)?.1 == io::json_hash(&v, "wal_sha256")?,
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
        catalog.source_hashes().database == database_sha
            && v["wal_sha256"] == json!(catalog.source_hashes().wal.map(|h| io::hex(&h)))
            && v["media_anchor"] == catalog.persisted_media_anchor()
            && v["track_count"] == catalog.tracks().len(),
        "saved Current SQL binding differs"
    );
    let anchor = anchor::parse(&cdb)?;
    check_current_control(control)?;
    ensure!(
        anchor.database_pid() == catalog.database_pid()
            && v["saved_cdb_media_anchor"] == anchor.saved_media_anchor()
            && v["saved_cdb_voice_memo_anchor"] == anchor.saved_voice_memo_anchor(),
        "saved CDB binding differs"
    );
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
        receipt,
    };
    check_current_control(control)?;
    Ok(loaded)
}
