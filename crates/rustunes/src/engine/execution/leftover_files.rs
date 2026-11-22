//! Song files on the phone that no song uses, which `sync` offers to remove.
//!
//! A file can land in a music folder after its song is gone: a song deleted
//! in the Music app while its file was still being copied keeps its
//! outstanding file, and an interrupted copy can leave one too (docs/04,
//! sections 4.6 and 9.3). No row names it, so it only takes space. iOS's
//! daily maintenance deletes such a file once it is more than 300 s old, but
//! only while the phone is idle. `sync` looks for them in its fresh capture,
//! after it has settled earlier work and before it sends anything, asks once,
//! and removes each one over a locked connection that can remove nothing
//! else, checking the file again right before. Declining, or failing to
//! look, never stops the sync.
use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use idevice::{IdeviceService, lockdown::LockdownClient, provider::IdeviceProvider};
use music_protocol::protocol::{
    afc::stat::Attributes,
    trace::private_write,
    transfer::{LeftoverRemoval, Transfer, leftover_media_path},
};
use serde::Serialize;
use serde_json::json;
use tokio::time::{Instant, timeout_at};

use crate::engine::{
    Engine, Event,
    execution::{
        device::{self, DeviceProvider},
        native_insert::create_trace,
    },
    state::{
        journal::{Journal, device_binding},
        snapshot::Snapshot,
    },
    verification::readiness::music_folder,
};

const MUSIC: &str = "iTunes_Control/Music";
/// How old a file must be by the phone's clock: iOS's own sweep waits as
/// long, so a file still being written is never taken.
const MINIMUM_AGE: Duration = Duration::from_secs(300);
/// How old by the host's clock, when the phone's cannot be read.
const HOST_MINIMUM_AGE: Duration = Duration::from_secs(3600);
/// AFC requests for one search or one removal: the folders, and a stat or a
/// removal for each file no track names.
const OPERATIONS: usize = 8192;

/// A song file no song uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LeftoverFile {
    /// Root-relative, `iTunes_Control/Music/Fnn/NAME.ext`.
    pub path: String,
    pub bytes: u64,
    /// Its last change, nanoseconds since 1970 by the phone's clock.
    pub modified: i128,
    /// The stat it was found with: only that same file is removed.
    #[serde(skip)]
    found: Attributes,
}

impl LeftoverFile {
    /// A file at `path` of `bytes`, as found, for the views' tests.
    #[cfg(test)]
    pub(crate) fn example(path: &str, bytes: u64) -> Self {
        Self {
            path: path.to_owned(),
            bytes,
            modified: 0,
            found: Attributes::new(),
        }
    }
}

/// The bytes `files` hold together.
pub fn total_bytes(files: &[LeftoverFile]) -> u64 {
    files.iter().map(|f| f.bytes).sum()
}

/// `sync`'s offer to remove the song files no song uses: asked at most once
/// a sync, however often it plans again.
pub struct LeftoverOffer<'a> {
    confirm: &'a (dyn Fn(&[LeftoverFile]) -> Result<bool> + Sync),
    asked: AtomicBool,
}

impl<'a> LeftoverOffer<'a> {
    /// `confirm` sees the files found and says whether to remove them. A
    /// dry run never asks: it reports them.
    pub fn new(confirm: &'a (dyn Fn(&[LeftoverFile]) -> Result<bool> + Sync)) -> Self {
        Self {
            confirm,
            asked: AtomicBool::new(false),
        }
    }

    /// True the first time only.
    fn first(&self) -> bool {
        !self.asked.swap(true, Ordering::Relaxed)
    }
}

/// Look for leftover song files on the phone `saved` shows, in its catalogue
/// and as it is now, and offer once to remove them: on a dry run only report
/// them. A path a removal in `journal` (when there is one) targeted is the
/// phone's to delete, which it does minutes after the session, so it is no
/// leftover. Nothing the journal holds is unfinished by then: `sync` settles
/// it first, and a dry run refuses it. A path the journal reserved for an
/// insertion is not excluded: a song deleted while its file was still being
/// copied leaves its file there. Evidence goes in `run/leftover-files`. Only stopping the
/// engine fails it; any other error is reported and the sync goes on.
pub(crate) async fn offer(
    offer: &LeftoverOffer<'_>,
    saved: &Snapshot,
    journal: &Path,
    run: &Path,
    dry_run: bool,
    engine: &Engine,
    seconds: u64,
) -> Result<()> {
    if !offer.first() {
        return Ok(());
    }
    let result = Box::pin(offer_once(
        offer, saved, journal, run, dry_run, engine, seconds,
    ))
    .await;
    match result {
        Err(error) if engine.check().is_ok() => {
            engine.emit(Event::LeftoverFilesUnchecked(&format!("{error:#}")))
        }
        result => result,
    }
}

async fn offer_once(
    offer: &LeftoverOffer<'_>,
    saved: &Snapshot,
    journal: &Path,
    run: &Path,
    dry_run: bool,
    engine: &Engine,
    seconds: u64,
) -> Result<()> {
    let evidence = run.join("leftover-files");
    create_trace(&evidence)?;
    let control = engine.control(seconds);
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(300));
    let provider = timeout_at(deadline, device::provider())
        .await
        .context("discovery deadline")??;
    ensure!(
        device_binding(&provider.udid)? == saved.identity.device_identifier_sha256,
        "the phone found is not the one this sync captured"
    );
    let (now, clock, minimum_age) = match phone_clock(&provider, deadline).await {
        Some(now) => (now, "phone", MINIMUM_AGE),
        None => (host_clock()?, "host", HOST_MINIMUM_AGE),
    };
    let mut named: BTreeSet<String> = saved
        .catalog
        .tracks()
        .iter()
        .filter_map(|t| t.afc_path.as_deref())
        .map(str::to_ascii_lowercase)
        .collect();
    if journal.join("journal.sqlite").try_exists()? {
        named
            .extend(Journal::open_read_only(journal, &saved.journal_identity()?)?.removed_paths()?);
    }
    let search = evidence.join("search");
    create_trace(&search)?;
    let mut afc = Transfer::connect_bounded(
        &provider,
        &search,
        deadline,
        OPERATIONS,
        control.cancelled.clone(),
    )
    .await?;
    let listed = unnamed_files(&mut afc, saved.catalog.music_folders(), &named).await?;
    drop(afc);
    let files = leftovers(listed, &named, now, minimum_age);
    private_write(
        &evidence.join("found.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema": "rustunes.leftover-files",
            "clock": clock,
            "now": now.to_string(),
            "minimum_age_seconds": minimum_age.as_secs(),
            "files": files,
        }))?,
    )?;
    if files.is_empty() {
        return Ok(());
    }
    if dry_run {
        return engine.emit(Event::LeftoverFilesFound(&files));
    }
    if !(offer.confirm)(&files)? {
        return Ok(());
    }
    let control = engine.control(seconds);
    let removal = evidence.join("removal");
    create_trace(&removal)?;
    let mut afc = Transfer::connect_bounded(
        &provider,
        &removal,
        control.deadline,
        OPERATIONS,
        control.cancelled.clone(),
    )
    .await?;
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut record = Vec::new();
    let mut stopped = None;
    for file in &files {
        engine.check()?;
        let outcome = match afc.remove_leftover_media(&file.path, &file.found).await {
            Ok(outcome) => outcome,
            Err(error) => {
                // The connection is spent: the rest stay for the next sync.
                stopped = Some(format!("{error:#}"));
                break;
            }
        };
        record.push(json!({"path": file.path, "bytes": file.bytes, "modified": file.modified.to_string(), "result": format!("{outcome:?}")}));
        match outcome {
            LeftoverRemoval::Removed => removed.push(file.clone()),
            LeftoverRemoval::Changed => kept.push(file.clone()),
            // The phone took it first.
            LeftoverRemoval::Gone => {}
        }
    }
    drop(afc);
    private_write(
        &evidence.join("removed.json"),
        &serde_json::to_vec_pretty(&json!({"files": record, "stopped": stopped}))?,
    )?;
    engine.emit(Event::LeftoverFilesRemoved {
        removed: &removed,
        kept: &kept,
    })?;
    match stopped {
        Some(error) => engine.emit(Event::LeftoverFilesUnchecked(&error)),
        None => Ok(()),
    }
}

/// Each file in the phone's music folders that no path in `named` (in lower
/// case) names, with its stat: Apple's `F00` to `F49`, and any other `Fnn`
/// folder the library registers (`registered`). Nothing else is listed.
async fn unnamed_files(
    afc: &mut Transfer,
    registered: &BTreeSet<String>,
    named: &BTreeSet<String>,
) -> Result<Vec<(String, Attributes)>> {
    let mut listed = Vec::new();
    for folder in folders(afc.names_if_present(MUSIC).await?, registered) {
        let is_directory = afc
            .stat(&folder)
            .await?
            .is_some_and(|a| a.get("st_ifmt").map(String::as_str) == Some("S_IFDIR"));
        if !is_directory {
            continue;
        }
        for name in afc.names(&folder).await? {
            let path = format!("{folder}/{name}");
            if name == "." || name == ".." || named.contains(&path.to_ascii_lowercase()) {
                continue;
            }
            if !leftover_media_path(&path) {
                continue;
            }
            if let Some(attributes) = afc.stat(&path).await? {
                listed.push((path, attributes));
            }
        }
    }
    Ok(listed)
}

/// The music folders among `names`, the entries of `iTunes_Control/Music`:
/// Apple's `F00` to `F49`, and any other `Fnn` the library registers.
fn folders(names: Vec<String>, registered: &BTreeSet<String>) -> Vec<String> {
    let mut folders: Vec<String> = names
        .into_iter()
        .filter(|name| music_folder(name))
        .map(|name| format!("{MUSIC}/{name}"))
        .filter(|path| {
            path[MUSIC.len() + 2..].parse::<u8>().is_ok_and(|n| n < 50) || registered.contains(path)
        })
        .collect();
    folders.sort();
    folders
}

/// The leftover song files among `listed`, each a path with its stat: a
/// regular file at a song-file path in a music folder
/// (`leftover_media_path`), named by no path in `named` (in lower case, as
/// the phone's names are compared), last changed at least `minimum_age`
/// before `now` (nanoseconds since 1970).
fn leftovers(
    listed: Vec<(String, Attributes)>,
    named: &BTreeSet<String>,
    now: i128,
    minimum_age: Duration,
) -> Vec<LeftoverFile> {
    let number = |attributes: &Attributes, key: &str| {
        attributes
            .get(key)
            .and_then(|value| value.parse::<i128>().ok())
    };
    listed
        .into_iter()
        .filter_map(|(path, found)| {
            let regular = found.get("st_ifmt").map(String::as_str) == Some("S_IFREG");
            let bytes = u64::try_from(number(&found, "st_size")?).ok()?;
            let modified = number(&found, "st_mtime")?;
            let old = now - modified >= minimum_age.as_nanos() as i128;
            (regular
                && old
                && leftover_media_path(&path)
                && !named.contains(&path.to_ascii_lowercase()))
            .then_some(LeftoverFile {
                path,
                bytes,
                modified,
                found,
            })
        })
        .collect()
}

/// The phone's clock, nanoseconds since 1970, from Lockdown's
/// `TimeIntervalSince1970`; None when it cannot be read in time.
async fn phone_clock(provider: &DeviceProvider, deadline: Instant) -> Option<i128> {
    let bound = deadline.min(Instant::now() + Duration::from_secs(15));
    let seconds = timeout_at(bound, async {
        let pairing = provider.get_pairing_file().await?;
        let mut lockdown = LockdownClient::connect(provider).await?;
        lockdown.start_session(&pairing).await?;
        let value = lockdown
            .get_value(Some("TimeIntervalSince1970"), None)
            .await?;
        anyhow::Ok(
            value
                .as_real()
                .or_else(|| value.as_signed_integer().map(|n| n as f64)),
        )
    })
    .await
    .ok()?
    .ok()??;
    (seconds.is_finite() && seconds > 0.0).then_some((seconds * 1e9) as i128)
}

fn host_clock() -> Result<i128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i128)
}

#[cfg(test)]
mod tests;
