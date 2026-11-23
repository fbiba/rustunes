//! The phone's state files as one capture saw them, held unchanged by the
//! session or readback that relies on them.
//!
//! A `HeldState` is built from a `Current` capture. It binds each file
//! rustunes reads before it writes, with one rule per file: the prefs pair
//! and the SQL catalogue by their exact bytes, the catalogue's log by its
//! bytes or its absence, its shared-memory index by its stat alone, the
//! iTunesCDB a computer left by its bytes once it qualifies (or its absence,
//! on a phone `rustunes init` prepared), and, when the caller asks, the
//! ArtworkDB by its bytes or its absence. The same binding answers what a
//! new insert may use (occupied identities and paths, the database prefix,
//! the ancillary snapshot) and builds the session's templates.
//!
//! One guard holds every file in one pair of stats: it stats them all,
//! checks each against its rule and reads every bound one back whole, and
//! stats them all again. A file that differs gives a typed
//! `StateReadDrift` naming it. The guard reports its own cost
//! (`guard_requests`, `guard_read_bytes`), so a caller sizes a connection
//! without knowing which files are held.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::engine::state::{current::Current, device_files, journal::Identity};
use music_protocol::catalogue::identity::{self, LibraryIdentity};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::ancillary::EligibleSnapshot;
use music_protocol::protocol::templates::{self, QUALIFIED_VOICE_MEMO_ANCHOR, Templates};
use music_protocol::protocol::transfer::{
    Attributes, MAX_ASSET, MAX_CATALOG, ReadbackChanged, ReadbackPathAbsent, Transfer,
    regular_size, same_stat,
};

/// The largest prefs file held.
const PREFS_CAP: u64 = 1024 * 1024;
/// The bytes per read request the cost counts. A read asks for up to 1 MiB
/// at a time; counting a request per 64 KiB also covers a phone that answers
/// each with as little as that.
const CHUNK: u64 = 64 * 1024;

/// Whether a binding holds the ArtworkDB. A session that allocates cover
/// cache IDs from it, or that must leave it as it was (a removal or metadata
/// session, a cover update), holds it; others leave it out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtworkDb {
    Held,
    NotHeld,
}

/// What a guard requires of one held file.
enum Rule {
    /// These bytes: a regular file of this size, at most `cap`, whose whole
    /// contents read back with this SHA-256.
    Bytes {
        bytes: u64,
        sha256: [u8; 32],
        cap: u64,
    },
    /// Nothing at the path.
    Absent,
    /// Any file or none, whose stat does not change across the guard: the
    /// catalogue's shared-memory index, which SQLite rewrites as it reads.
    Stat,
}

struct HeldFile {
    path: &'static str,
    rule: Rule,
}

impl HeldFile {
    fn bytes(path: &'static str, bytes: &[u8], cap: u64) -> Self {
        Self {
            path,
            rule: Rule::Bytes {
                bytes: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
                cap,
            },
        }
    }
    fn name(&self) -> &'static str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }
    fn drift(&self, drift: Drift) -> anyhow::Error {
        StateReadDrift {
            file: self.name(),
            drift,
        }
        .into()
    }
}

pub(crate) struct HeldState {
    device: String,
    identity: LibraryIdentity,
    /// In `device_files` order: prefs, plist, catalogue, log, index, CDB,
    /// then the ArtworkDB when held.
    files: Vec<HeldFile>,
    /// The bound CDB's saved VoiceMemo anchor, or the qualified one with none.
    voice_memo: u32,
    anchor: u32,
    database_pid: u64,
    container_pid: u64,
    occupied: BTreeSet<u64>,
    paths: BTreeSet<String>,
    input_receipt_sha256: [u8; 32],
    ancillary: EligibleSnapshot,
}

impl HeldState {
    /// The binding of `current`, whose every track has a file location.
    /// `input_receipt_sha256` is the receipt of the input that bound these
    /// observations and decided what to send (a verification passes the
    /// capture's own); the caller keeps its bytes durably. A refusal names
    /// what does not qualify: a prefs pair that disagrees or exceeds its
    /// bound, an oversized catalogue or log, a CDB that belongs to another
    /// catalogue or holds another VoiceMemo anchor, an empty or oversized
    /// ArtworkDB, or a CDB or held ArtworkDB the capture did not look for.
    pub(crate) fn new(
        current: &Current,
        input_receipt_sha256: [u8; 32],
        artwork: ArtworkDb,
    ) -> Result<Self> {
        Self::with_unfilled(current, input_receipt_sha256, artwork, &BTreeSet::new())
    }

    /// `new`, where exactly the rows `unfilled` (rows an unfinished insertion
    /// left) have no file location; every other row's path is bound.
    pub(crate) fn with_unfilled(
        current: &Current,
        input_receipt_sha256: [u8; 32],
        artwork: ArtworkDb,
        unfilled: &BTreeSet<u64>,
    ) -> Result<Self> {
        let catalog = &current.catalog;
        let paths = bound_paths(catalog, unfilled)?;
        ensure!(
            input_receipt_sha256 != [0; 32],
            "missing current-state/matching receipt"
        );
        let identity = identity::parse(&current.raw_preferences)?;
        identity.ensure_matches(&identity::parse(&current.plist_preferences)?)?;
        Identity::new(&current.device, &identity)?;
        let mut files = Vec::with_capacity(7);
        for (path, bytes) in [
            (device_files::PREFS, &current.raw_preferences),
            (device_files::PREFS_PLIST, &current.plist_preferences),
        ] {
            ensure!(
                !bytes.is_empty() && bytes.len() as u64 <= PREFS_CAP,
                "preferences exceed bounded state input"
            );
            files.push(HeldFile::bytes(path, bytes, PREFS_CAP));
        }
        let hashes = catalog.source_hashes();
        ensure!(
            hashes.database_bytes > 0 && hashes.database_bytes <= MAX_CATALOG,
            "catalog database exceeds insertion state bound"
        );
        files.push(HeldFile {
            path: device_files::DATABASE,
            rule: Rule::Bytes {
                bytes: hashes.database_bytes,
                sha256: hashes.database,
                cap: MAX_CATALOG,
            },
        });
        files.push(HeldFile {
            path: device_files::DATABASE_WAL,
            rule: match (hashes.wal, hashes.wal_bytes) {
                (Some(sha256), Some(bytes)) if bytes <= MAX_CATALOG => Rule::Bytes {
                    bytes,
                    sha256,
                    cap: MAX_CATALOG,
                },
                (None, None) => Rule::Absent,
                _ => anyhow::bail!(
                    "catalog WAL exceeds insertion state bound or has inconsistent presence"
                ),
            },
        });
        files.push(HeldFile {
            path: device_files::DATABASE_SHM,
            rule: Rule::Stat,
        });
        // Its saved VoiceMemo anchor must be the qualified one and its
        // database PID the SQL catalogue's. Its Media anchor is stale and
        // never replaces the catalogue's.
        let cdb = current.cdb.captured_bytes("iTunesCDB")?;
        let voice_memo = templates::copied_voice_memo_anchor(cdb, catalog.database_pid())?;
        ensure!(
            voice_memo == QUALIFIED_VOICE_MEMO_ANCHOR,
            "copied CDB has an unqualified saved VoiceMemo anchor"
        );
        files.push(match cdb {
            Some(bytes) => HeldFile::bytes(device_files::CDB, bytes, MAX_CATALOG),
            None => HeldFile {
                path: device_files::CDB,
                rule: Rule::Absent,
            },
        });
        if artwork == ArtworkDb::Held {
            files.push(
                match current.artwork_db.captured_bytes("the copied ArtworkDB")? {
                    Some(bytes) => {
                        ensure!(
                            !bytes.is_empty() && bytes.len() as u64 <= MAX_ASSET,
                            "ArtworkDB exceeds bound"
                        );
                        HeldFile::bytes(device_files::ARTWORK_DB, bytes, MAX_ASSET)
                    }
                    None => HeldFile {
                        path: device_files::ARTWORK_DB,
                        rule: Rule::Absent,
                    },
                },
            );
        }
        Ok(Self {
            device: current.device.clone(),
            ancillary: EligibleSnapshot::new(catalog, &identity),
            identity,
            files,
            voice_memo,
            anchor: catalog.persisted_media_anchor(),
            database_pid: catalog.database_pid(),
            container_pid: catalog.primary_container_pid(),
            occupied: catalog.represented_sync_ids().clone(),
            paths,
            input_receipt_sha256,
        })
    }

    pub(crate) fn device_identifier(&self) -> &str {
        &self.device
    }
    pub(crate) fn library_identity(&self) -> &LibraryIdentity {
        &self.identity
    }
    /// The catalogue's persisted Media anchor.
    pub(crate) fn media_anchor(&self) -> u32 {
        self.anchor
    }
    pub(crate) fn database_pid(&self) -> u64 {
        self.database_pid
    }
    pub(crate) fn container_pid(&self) -> u64 {
        self.container_pid
    }
    /// Every identity the catalogue represents.
    pub(crate) fn occupied(&self) -> &BTreeSet<u64> {
        &self.occupied
    }
    /// Every file location the catalogue refers to.
    pub(crate) fn paths(&self) -> &BTreeSet<String> {
        &self.paths
    }
    pub(crate) fn input_receipt_sha256(&self) -> [u8; 32] {
        self.input_receipt_sha256
    }
    pub(crate) fn ancillary(&self) -> &EligibleSnapshot {
        &self.ancillary
    }
    pub(crate) fn ensure_catalog(&self, catalog: &Catalog) -> Result<()> {
        self.ancillary.ensure_current(catalog, &self.identity)
    }
    pub(crate) fn holds_artwork(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.path == device_files::ARTWORK_DB)
    }
    /// The bound iTunesCDB's size and SHA-256, or None for its absence.
    pub(crate) fn copied_cdb(&self) -> Option<(u64, [u8; 32])> {
        self.bound(device_files::CDB)
    }
    /// The held ArtworkDB's size and SHA-256, or None when it is held absent
    /// or not held.
    pub(crate) fn artwork_db(&self) -> Option<(u64, [u8; 32])> {
        self.bound(device_files::ARTWORK_DB)
    }
    fn bound(&self, path: &str) -> Option<(u64, [u8; 32])> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .and_then(|file| match file.rule {
                Rule::Bytes { bytes, sha256, .. } => Some((bytes, sha256)),
                _ => None,
            })
    }

    /// The session's request and completion templates: this library's
    /// identity, the catalogue's Media anchor and the bound CDB's VoiceMemo
    /// anchor, with `host_name`.
    pub(crate) fn templates(&self, host_name: &str) -> Result<Templates> {
        templates::build(&self.identity, host_name, self.anchor, self.voice_memo)
    }

    /// An upper bound of the AFC requests one guard makes: two stats of each
    /// held file and, for each bound one, a stat, an open, its reads, a close
    /// and a stat. Requests of `guard_around`'s inside are its caller's.
    pub(crate) fn guard_requests(&self) -> u64 {
        self.files
            .iter()
            .map(|file| match file.rule {
                Rule::Bytes { bytes, .. } => 6 + bytes.div_ceil(CHUNK),
                Rule::Absent | Rule::Stat => 2,
            })
            .sum()
    }

    /// An upper bound of the bytes one guard's reads return, counted in the
    /// same whole requests as `guard_requests`.
    pub(crate) fn guard_read_bytes(&self) -> u64 {
        self.files
            .iter()
            .map(|file| match file.rule {
                Rule::Bytes { bytes, .. } => bytes.div_ceil(CHUNK) * CHUNK,
                Rule::Absent | Rule::Stat => 0,
            })
            .sum()
    }

    /// Hold every file: stat each, check each against its rule (reading every
    /// bound file back whole), and stat each again. A file that differs is a
    /// `StateReadDrift` naming it; a malformed, oversized or unreadable one
    /// is an ordinary refusal.
    pub(crate) async fn guard(&self, afc: &mut Transfer) -> Result<()> {
        self.guard_around(afc, async |_| Ok(())).await
    }

    /// `guard`, running `inside` after the reads and before the closing
    /// stats, so the pair of stats also holds every file while it runs. Its
    /// error is returned as it is, before the closing stats.
    pub(crate) async fn guard_around<T>(
        &self,
        afc: &mut Transfer,
        inside: impl AsyncFnOnce(&mut Transfer) -> Result<T>,
    ) -> Result<T> {
        let mut before = Vec::with_capacity(self.files.len());
        for file in &self.files {
            before.push(afc.stat(file.path).await?);
        }
        for (file, before) in self.files.iter().zip(&before) {
            match (&file.rule, before) {
                (Rule::Bytes { .. }, None) => return Err(file.drift(Drift::Disappeared)),
                (Rule::Bytes { bytes, sha256, cap }, Some(attributes)) => {
                    check_size(attributes, file, *bytes, *cap)?;
                    let read = afc.read_hash(file.path, *bytes, *cap).await;
                    if read.map_err(|error| changed_under_read(file, error))? != *sha256 {
                        return Err(file.drift(Drift::Digest));
                    }
                }
                (Rule::Absent, Some(_)) => return Err(file.drift(Drift::Appeared)),
                (Rule::Absent, None) | (Rule::Stat, _) => {}
            }
        }
        let value = inside(afc).await?;
        for (file, before) in self.files.iter().zip(&before) {
            if !same_stat(before.as_ref(), afc.stat(file.path).await?.as_ref()) {
                return Err(file.drift(Drift::Attributes));
            }
        }
        Ok(value)
    }
}

/// A held file's readback that failed because the file changed between the
/// guard's stat and the read, or during it (a song ended), is drift; any
/// other failure stays as it is.
fn changed_under_read(file: &HeldFile, error: anyhow::Error) -> anyhow::Error {
    if error
        .chain()
        .any(|c| c.downcast_ref::<ReadbackChanged>().is_some())
    {
        file.drift(Drift::Attributes)
    } else if error
        .chain()
        .any(|c| c.downcast_ref::<ReadbackPathAbsent>().is_some())
    {
        file.drift(Drift::Disappeared)
    } else {
        error
    }
}

/// Every file location `catalog` refers to, when exactly the rows `unfilled`
/// have none.
fn bound_paths(catalog: &Catalog, unfilled: &BTreeSet<u64>) -> Result<BTreeSet<String>> {
    if unfilled.is_empty() {
        return catalog.music_paths();
    }
    let mut paths = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for track in catalog.tracks() {
        match &track.afc_path {
            Some(path) => {
                ensure!(paths.insert(path.clone()), "duplicate catalog track path");
            }
            None => {
                pending.insert(track.sync_id);
            }
        }
    }
    ensure!(
        pending == *unfilled,
        "only the rows an unfinished insertion left may lack a file location"
    );
    Ok(paths)
}

/// Check a bound file's stat before its read. A stat of an unsupported shape
/// (not a regular file, a size or modification time missing or malformed, a
/// size over `cap`) is an ordinary refusal, never drift; only a well-formed
/// stat of another size is.
fn check_size(attributes: &Attributes, file: &HeldFile, expected: u64, cap: u64) -> Result<()> {
    let actual: u64 = attributes
        .get("st_size")
        .context("missing state size")?
        .parse()?;
    regular_size(attributes, actual)?;
    let _: u64 = attributes
        .get("st_mtime")
        .context("missing state modification time")?
        .parse()?;
    ensure!(actual <= cap, "observed state file exceeds bound");
    if actual != expected {
        return Err(file.drift(Drift::Size));
    }
    Ok(())
}

/// How a held file differed from its binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Drift {
    /// A bound file has another size.
    Size,
    /// A bound file has other bytes.
    Digest,
    /// A file bound absent is there.
    Appeared,
    /// A bound file is not there.
    Disappeared,
    /// A held file's stat changed between the guard's two stats.
    Attributes,
}

/// A guard found a held file other than its binding.
#[derive(Debug)]
pub(crate) struct StateReadDrift {
    file: &'static str,
    drift: Drift,
}

impl StateReadDrift {
    #[cfg(test)]
    pub(crate) fn fixture(file: &'static str, drift: Drift) -> Self {
        Self { file, drift }
    }
}

impl std::fmt::Display for StateReadDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.drift {
            Drift::Size => "regular file size changed",
            Drift::Digest => "complete file digest changed",
            Drift::Appeared => "appeared after it was captured absent",
            Drift::Disappeared => "is absent although it was captured",
            Drift::Attributes => "attributes changed across the guard",
        };
        write!(f, "state read drift in {}: {reason}", self.file)
    }
}

impl std::error::Error for StateReadDrift {}

/// Whether `error` came from a guard finding any held file other than its
/// binding: the phone wrote its state after the capture (a song starting or
/// ending writes the catalogue). Before Started, nothing was sent, so the
/// step may start again from a fresh capture.
pub(crate) fn is_drift(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<StateReadDrift>().is_some())
}

/// A binding with the given catalogue facts, for tests of the checks that
/// read them; its files are the prefs pair and a small catalogue.
#[cfg(test)]
pub(crate) struct Fixture {
    pub(crate) device: String,
    pub(crate) identity: LibraryIdentity,
    pub(crate) anchor: u32,
    pub(crate) database_pid: u64,
    pub(crate) container_pid: u64,
    pub(crate) occupied: BTreeSet<u64>,
    pub(crate) paths: BTreeSet<String>,
    pub(crate) input_receipt_sha256: [u8; 32],
    pub(crate) ancillary: EligibleSnapshot,
}

#[cfg(test)]
impl Fixture {
    pub(crate) fn state(self) -> HeldState {
        self.with_sizes([Some(1232), Some(2000), Some(4096), None, None, None])
    }

    /// The binding of the largest files each rule allows: the prefs pair, the
    /// catalogue and its log, the CDB when `cdb`, and the ArtworkDB when
    /// `artwork`.
    pub(crate) fn largest(self, cdb: bool, artwork: bool) -> HeldState {
        let catalogue = Some(MAX_CATALOG);
        let cdb = cdb.then_some(MAX_CATALOG);
        let artwork = artwork.then_some(Some(MAX_ASSET));
        let held = Some(PREFS_CAP);
        self.with_sizes([held, held, catalogue, catalogue, cdb, artwork.flatten()])
    }

    /// Bound files of these sizes, in order prefs, plist, catalogue, log,
    /// CDB and ArtworkDB; None binds a file's absence (the ArtworkDB's: not
    /// held).
    fn with_sizes(self, sizes: [Option<u64>; 6]) -> HeldState {
        let rule = |size: Option<u64>, cap| match size {
            Some(bytes) => Rule::Bytes {
                bytes,
                sha256: [1; 32],
                cap,
            },
            None => Rule::Absent,
        };
        let [prefs, plist, database, wal, cdb, artwork] = sizes;
        let mut files = vec![
            HeldFile {
                path: device_files::PREFS,
                rule: rule(prefs, PREFS_CAP),
            },
            HeldFile {
                path: device_files::PREFS_PLIST,
                rule: rule(plist, PREFS_CAP),
            },
            HeldFile {
                path: device_files::DATABASE,
                rule: rule(database, MAX_CATALOG),
            },
            HeldFile {
                path: device_files::DATABASE_WAL,
                rule: rule(wal, MAX_CATALOG),
            },
            HeldFile {
                path: device_files::DATABASE_SHM,
                rule: Rule::Stat,
            },
            HeldFile {
                path: device_files::CDB,
                rule: rule(cdb, MAX_CATALOG),
            },
        ];
        if artwork.is_some() {
            files.push(HeldFile {
                path: device_files::ARTWORK_DB,
                rule: rule(artwork, MAX_ASSET),
            });
        }
        HeldState {
            device: self.device,
            identity: self.identity,
            files,
            voice_memo: QUALIFIED_VOICE_MEMO_ANCHOR,
            anchor: self.anchor,
            database_pid: self.database_pid,
            container_pid: self.container_pid,
            occupied: self.occupied,
            paths: self.paths,
            input_receipt_sha256: self.input_receipt_sha256,
            ancillary: self.ancillary,
        }
    }
}

#[cfg(test)]
mod tests;
