//! Read-only inspection of what a phone holds for music syncing, and whether
//! rustunes can sync it, must first initialise it, or should leave it alone
//! (`rustunes device check`, and the first step of `rustunes init`).
//!
//! Nothing is written to the phone. The database is copied into the caller's
//! private trace directory.
use std::{collections::BTreeMap, os::unix::fs::DirBuilderExt, path::Path, time::Duration};

use anyhow::{Context, Result};
use idevice::{IdeviceService, lockdown::LockdownClient, provider::IdeviceProvider};
use music_protocol::{
    catalogue::{identity::LibraryIdentity, sqlite::LibraryState},
    protocol::{afc::stat::Attributes, transfer::Transfer},
};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout_at};

use crate::engine::{
    execution::device::DeviceProvider,
    state::{device_files, init_record::InitRecord},
    verification::catalogue_capture,
};

pub(crate) const SYNC_MEDIA: &str = "iTunes_Control/Sync/Media";
const MUSIC: &str = "iTunes_Control/Music";

/// Every path the report shows, in order.
const PATHS: [&str; 17] = [
    "iTunes_Control",
    "iTunes_Control/iTunes",
    device_files::PREFS,
    device_files::PREFS_PLIST,
    device_files::PREFS_TEMPORARY,
    device_files::PREFS_PLIST_TEMPORARY,
    device_files::CDB,
    device_files::DATABASE,
    device_files::DATABASE_WAL,
    device_files::DATABASE_SHM,
    "iTunes_Control/Artwork",
    device_files::ARTWORK_DB,
    MUSIC,
    "iTunes_Control/Sync",
    SYNC_MEDIA,
    "Airlock",
    "Airlock/Media",
];

/// What a write of a prefs file leaves when it stops before its rename.
const TEMPORARIES: [&str; 2] = [
    device_files::PREFS_TEMPORARY,
    device_files::PREFS_PLIST_TEMPORARY,
];

/// The largest prefs file, or prefs temporary, read from the phone.
const PREFS_CAP: u64 = 1024 * 1024;

/// The phone's library identity files.
pub(crate) enum Prefs {
    Absent,
    /// Only `present` of the pair is on the phone; `bytes` are its contents.
    Partial {
        present: &'static str,
        bytes: Vec<u8>,
    },
    /// Present, but not a configured `frpd` 1.25 identity, or the two differ.
    Invalid(String),
    /// `raw` is the phone's `iTunesPrefs`.
    Valid {
        identity: LibraryIdentity,
        raw: Vec<u8>,
    },
}

/// No Debug: the prefs hold the library identity.
pub(crate) struct Inspection {
    pub product_type: String,
    pub product_version: String,
    pub build_version: String,
    pub device_name: String,
    pub paths: Vec<(&'static str, Option<Attributes>)>,
    /// The names in `iTunes_Control/Music`, when it exists.
    pub music: Option<Vec<String>>,
    /// The names in `iTunes_Control/Sync/Media`, when it exists.
    pub staged: Option<Vec<String>>,
    pub prefs: Prefs,
    /// The prefs temporaries on the phone, each with its contents (None: not
    /// a regular file of at most 1 MiB).
    pub temporaries: Vec<(&'static str, Option<Vec<u8>>)>,
    /// None when the phone has no database yet.
    pub library: Option<LibraryState>,
}

/// What rustunes can do with the phone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// A library owns the phone and its database records a sync: rustunes
    /// syncs it. `initialised` says the library is the one `rustunes init`
    /// created from this state directory.
    Synced { initialised: bool },
    /// As `Synced`, but some track lacks a row in a table beside `item`, and
    /// the catalogue refuses such a library. `reason` names the tables and
    /// the counts.
    Inconsistent { initialised: bool, reason: String },
    /// As `Synced`, but `iTunes_Control/Sync/Media` holds `staged`: files a
    /// sync left when it stopped before the phone took them. Every write
    /// refuses while they are there. The command that left rustunes' own
    /// removes them when it runs again; nothing in rustunes removes another
    /// host's.
    Interrupted {
        initialised: bool,
        staged: Vec<String>,
    },
    /// No computer has synced the phone: `rustunes init` prepares it.
    NeverSynced,
    /// `rustunes init` began writing its prefs, but its first session did not
    /// finish: a rerun of `init` finishes it.
    InitUnfinished,
    /// Neither; each reason says what does not fit.
    Unclear(Vec<String>),
}

impl Inspection {
    pub fn state(&self, path: &str) -> Option<&Attributes> {
        self.paths
            .iter()
            .find(|(name, _)| *name == path)
            .and_then(|(_, attributes)| attributes.as_ref())
    }

    pub fn present(&self, path: &str) -> bool {
        self.state(path).is_some()
    }

    /// The pair `record` wrote, when the phone's `iTunesPrefs` is exactly its
    /// first file, with or without the plist beside it.
    pub fn own_prefs(
        &self,
        record: &InitRecord,
    ) -> Option<music_protocol::catalogue::prefs::Prefs> {
        let raw = match &self.prefs {
            Prefs::Partial {
                present: device_files::PREFS,
                bytes,
            } => bytes,
            Prefs::Valid { raw, .. } => raw,
            _ => return None,
        };
        record.written_prefs(raw).filter(|prefs| prefs.raw == *raw)
    }

    /// Each prefs temporary on the phone, with what `record`'s `init` was
    /// writing through it when it stopped: None for one `init` did not leave,
    /// or with no record. An own temporary holds those bytes or their start;
    /// the plist's can only follow `init`'s own `iTunesPrefs`.
    pub fn leftover_temporaries(
        &self,
        record: Option<&InitRecord>,
    ) -> Vec<(&'static str, Option<Vec<u8>>)> {
        self.temporaries
            .iter()
            .map(|(path, found)| {
                let writing = record.zip(found.as_ref()).and_then(|(record, found)| {
                    let writing = match *path {
                        device_files::PREFS_TEMPORARY => record.written_prefs(found)?.raw,
                        device_files::PREFS_PLIST_TEMPORARY => self.own_prefs(record)?.plist,
                        _ => return None,
                    };
                    writing.starts_with(found).then_some(writing)
                });
                (*path, writing)
            })
            .collect()
    }

    /// Judge the phone, given the record of the library `rustunes init`
    /// created for it, if any.
    pub fn readiness(&self, record: Option<&InitRecord>) -> Result<Readiness> {
        // `init` writes iTunesPrefs, then its plist: it may have stopped
        // between the two.
        let own = match (&self.prefs, record) {
            (Prefs::Valid { identity, .. }, Some(record)) => record.owns(identity)?,
            (Prefs::Partial { .. }, Some(record)) => self.own_prefs(record).is_some(),
            _ => false,
        };
        let library_synced = self.library.as_ref().is_some_and(|library| {
            library.db_info_rows == 1 && library.media_anchor.as_deref().is_some_and(|a| a != "0")
        });
        let library_fresh = self.library.as_ref().is_none_or(LibraryState::never_synced);
        let inconsistency = self
            .library
            .as_ref()
            .and_then(|library| library.item_rows.inconsistency());
        let mut reasons = Vec::new();
        match &self.prefs {
            Prefs::Valid { .. } if library_synced => {
                let staged: Vec<String> = self.staged.iter().flatten().cloned().collect();
                return Ok(match inconsistency {
                    Some(reason) => Readiness::Inconsistent {
                        initialised: own,
                        reason,
                    },
                    None if !staged.is_empty() => Readiness::Interrupted {
                        initialised: own,
                        staged,
                    },
                    None => Readiness::Synced { initialised: own },
                });
            }
            Prefs::Valid { .. } if own && library_fresh => {}
            Prefs::Valid { .. } if library_fresh => reasons.push(
                "the phone holds another computer's library identity, but its database records \
                 no finished sync of it: let that computer finish syncing the phone"
                    .to_owned(),
            ),
            Prefs::Valid { .. } => reasons.push(
                "the phone holds a library identity, but its database is neither empty nor \
                 synced (no single db_info row and Media anchor)"
                    .to_owned(),
            ),
            Prefs::Partial { .. } if own && library_fresh => {}
            Prefs::Partial { present, .. } => reasons.push(format!(
                "only {present} of the library identity pair is on the phone"
            )),
            Prefs::Invalid(error) => {
                reasons.push(format!("the phone's library identity is unusable: {error}"))
            }
            Prefs::Absent if !library_fresh => reasons.push(
                "the database records a sync, but the phone holds no library identity".to_owned(),
            ),
            Prefs::Absent => {}
        }
        if let Some(inconsistency) = inconsistency {
            reasons.push(format!(
                "the music library is inconsistent: {inconsistency}"
            ));
        }
        for path in [device_files::CDB, device_files::ARTWORK_DB] {
            if self.present(path) && !library_synced {
                reasons.push(format!("{path} is on the phone, but no sync is recorded"));
            }
        }
        let temporaries = self.leftover_temporaries(record);
        for (path, writing) in &temporaries {
            if writing.is_none() {
                reasons.push(format!(
                    "{path} is on the phone: a write of the library identity stopped, \
                     and rustunes init did not make it"
                ));
            }
        }
        let foreign_staged: Vec<_> = self
            .staged
            .iter()
            .flatten()
            .filter(|name| {
                !own || !["Sync_00000000.plist", "Sync_00000000.plist.cig"].contains(&name.as_str())
            })
            .collect();
        if !foreign_staged.is_empty() {
            reasons.push(format!(
                "{SYNC_MEDIA} holds files another sync left: {}",
                foreign_staged
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        // Without reasons, every temporary left is init's own.
        Ok(if !reasons.is_empty() {
            Readiness::Unclear(reasons)
        } else if own || !temporaries.is_empty() {
            Readiness::InitUnfinished
        } else {
            Readiness::NeverSynced
        })
    }

    /// The report, without the identity itself: only whether it is there and
    /// whether it is the one `rustunes init` created.
    pub fn report(&self, readiness: &Readiness) -> Value {
        let paths: Vec<_> = self
            .paths
            .iter()
            .map(|(path, attributes)| {
                json!({
                    "path": path,
                    "present": attributes.is_some(),
                    "kind": attributes.as_ref().and_then(|a| a.get("st_ifmt")).map(|kind| match kind.as_str() {
                        "S_IFDIR" => "directory",
                        "S_IFREG" => "file",
                        _ => "other",
                    }),
                    "bytes": attributes.as_ref().filter(|a| a.get("st_ifmt").map(String::as_str) == Some("S_IFREG"))
                        .and_then(|a| a.get("st_size")).and_then(|size| size.parse::<u64>().ok()),
                })
            })
            .collect();
        let prefs = match &self.prefs {
            Prefs::Absent => json!({"state": "absent"}),
            Prefs::Partial { present, .. } => json!({"state": "partial", "present": present}),
            Prefs::Invalid(error) => json!({"state": "invalid", "error": error}),
            Prefs::Valid { .. } => json!({"state": "valid"}),
        };
        let library = self.library.as_ref().map(|library| {
            json!({
                "user_version": library.user_version,
                "items": library.item_rows.items,
                "companion_rows": library.item_rows.companions.iter().map(|c| (c.table.to_owned(), json!({"rows": c.rows, "missing": c.missing, "stray": c.stray}))).collect::<serde_json::Map<_, _>>(),
                "leftover_songs": library.item_rows.stray_tracks,
                "songs_until_cleared": library.item_rows.songs_until_cleared(),
                "synced_rows": library.synced_rows.iter().map(|(table, rows)| (table.to_string(), json!(rows))).collect::<serde_json::Map<_, _>>(),
                "db_info_rows": library.db_info_rows,
                "media_anchor": library.media_anchor,
                "playlists": library.containers.len(),
            })
        });
        json!({
            "schema": "rustunes.device-check",
            "device": {
                "product_type": self.product_type,
                "product_version": self.product_version,
                "build_version": self.build_version,
                "name": self.device_name,
            },
            "paths": paths,
            "music_folders": self.music.as_ref().map(|names| names.iter().filter(|name| music_folder(name)).count()),
            "staged": self.staged,
            "library_identity": prefs,
            "database": library,
            "readiness": match readiness {
                Readiness::Synced { initialised } => json!({"state": "synced", "initialised_by_rustunes": initialised}),
                Readiness::Inconsistent { initialised, reason } => json!({"state": "inconsistent", "initialised_by_rustunes": initialised, "reason": reason}),
                Readiness::Interrupted { initialised, staged } => json!({"state": "interrupted_sync", "initialised_by_rustunes": initialised, "staged": staged}),
                Readiness::NeverSynced => json!({"state": "never_synced"}),
                Readiness::InitUnfinished => json!({"state": "init_unfinished"}),
                Readiness::Unclear(reasons) => json!({"state": "unclear", "reasons": reasons}),
            },
            "phone_writes": 0,
        })
    }
}

/// A directory listing without its `.` and `..`.
fn entries(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| name != "." && name != "..")
        .collect()
}

/// `Fnn`, as Apple names the Music folders.
pub(crate) fn music_folder(name: &str) -> bool {
    name.len() == 3 && name.starts_with('F') && name[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Inspect the phone. `trace` must be a new path; it becomes a private
/// directory holding the database copy and AFC evidence.
pub(crate) async fn inspect(
    provider: &DeviceProvider,
    trace: &Path,
    deadline: Instant,
) -> Result<Inspection> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(trace)
        .context("inspection trace directory must be new")?;
    let details = lockdown_details(provider, deadline).await?;
    let afc_trace = trace.join("afc");
    std::fs::DirBuilder::new().mode(0o700).create(&afc_trace)?;
    let mut afc = Transfer::connect(provider, &afc_trace, deadline).await?;
    let mut paths = Vec::new();
    for path in PATHS {
        paths.push((path, afc.stat(path).await?));
    }
    let is_directory = |path: &str| {
        paths
            .iter()
            .find(|(name, _)| *name == path)
            .and_then(|(_, attributes)| attributes.as_ref())
            .and_then(|attributes| attributes.get("st_ifmt"))
            .is_some_and(|kind| kind == "S_IFDIR")
    };
    let music = if is_directory(MUSIC) {
        Some(entries(afc.names(MUSIC).await?))
    } else {
        None
    };
    let staged = if is_directory(SYNC_MEDIA) {
        Some(entries(afc.names(SYNC_MEDIA).await?))
    } else {
        None
    };
    let attributes = |path: &str| {
        paths
            .iter()
            .find(|(name, _)| *name == path)
            .and_then(|(_, a)| a.clone())
    };
    let mut copies = BTreeMap::new();
    for path in [device_files::PREFS, device_files::PREFS_PLIST] {
        let Some(attributes) = attributes(path) else {
            continue;
        };
        let size: u64 = attributes
            .get("st_size")
            .context("prefs size missing")?
            .parse()?;
        anyhow::ensure!(size <= PREFS_CAP, "{path} exceeds 1 MiB");
        let mut bytes = Vec::new();
        afc.read_exact(path, size, PREFS_CAP, &mut bytes).await?;
        copies.insert(path, bytes);
    }
    let mut temporaries = Vec::new();
    for path in TEMPORARIES {
        let Some(attributes) = attributes(path) else {
            continue;
        };
        let size = attributes
            .get("st_size")
            .and_then(|size| size.parse::<u64>().ok())
            .filter(|size| *size <= PREFS_CAP)
            .filter(|_| attributes.get("st_ifmt").map(String::as_str) == Some("S_IFREG"));
        let bytes = match size {
            Some(size) => {
                let mut bytes = Vec::new();
                afc.read_exact(path, size, PREFS_CAP, &mut bytes).await?;
                Some(bytes)
            }
            None => None,
        };
        temporaries.push((path, bytes));
    }
    drop(afc);
    let prefs = match (
        copies.remove(device_files::PREFS),
        copies.remove(device_files::PREFS_PLIST),
    ) {
        (None, None) => Prefs::Absent,
        (Some(bytes), None) => Prefs::Partial {
            present: device_files::PREFS,
            bytes,
        },
        (None, Some(bytes)) => Prefs::Partial {
            present: device_files::PREFS_PLIST,
            bytes,
        },
        (Some(raw), Some(plist)) => {
            let parsed = music_protocol::catalogue::identity::parse(&raw).and_then(|identity| {
                identity.ensure_matches(&music_protocol::catalogue::identity::parse(&plist)?)?;
                Ok(identity)
            });
            match parsed {
                Ok(identity) => Prefs::Valid { identity, raw },
                Err(error) => Prefs::Invalid(format!("{error:#}")),
            }
        }
    };
    let library = if paths
        .iter()
        .any(|(name, attributes)| *name == device_files::DATABASE && attributes.is_some())
    {
        catalogue_capture::library_state(provider, &trace.join("database"), deadline).await?
    } else {
        None
    };
    Ok(Inspection {
        product_type: details.0,
        product_version: details.1,
        build_version: details.2,
        device_name: details.3,
        paths,
        music,
        staged,
        prefs,
        temporaries,
        library,
    })
}

async fn lockdown_details(
    provider: &DeviceProvider,
    deadline: Instant,
) -> Result<(String, String, String, String)> {
    let bound = deadline.min(Instant::now() + Duration::from_secs(15));
    timeout_at(bound, async {
        let pairing = provider.get_pairing_file().await?;
        let mut lockdown = LockdownClient::connect(provider)
            .await
            .context("connecting to Lockdown")?;
        lockdown
            .start_session(&pairing)
            .await
            .context("starting existing paired Lockdown session")?;
        let mut values = Vec::new();
        for key in [
            "ProductType",
            "ProductVersion",
            "BuildVersion",
            "DeviceName",
        ] {
            let value = lockdown
                .get_value(Some(key), None)
                .await
                .with_context(|| format!("reading Lockdown {key}"))?;
            values.push(
                value
                    .as_string()
                    .with_context(|| format!("Lockdown {key} is not a string"))?
                    .to_owned(),
            );
        }
        let mut values = values.into_iter();
        Ok((
            values.next().unwrap(),
            values.next().unwrap(),
            values.next().unwrap(),
            values.next().unwrap(),
        ))
    })
    .await
    .context("Lockdown details deadline")?
}

#[cfg(test)]
pub(crate) mod tests;
