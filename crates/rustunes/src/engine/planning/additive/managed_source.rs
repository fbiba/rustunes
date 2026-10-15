//! Shared fixed bounded managed-run lookup; never chooses latest history.
use crate::{
    engine::execution::native_insert::Control,
    engine::state::journal::{
        Identity, InsertBatchId, InsertBatchRecord, Journal, row_ancestry as ancestry,
    },
};
use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
const MAX_RUNS: usize = 4096;
const MAX_ENTRIES: usize = 32768;
pub(super) const MAX_INPUT: u64 = 1024 * 1024;
pub(super) const MAX_PLAN: u64 = 4 * 1024 * 1024;
const MAX_INPUTS_TOTAL: u64 = 64 * 1024 * 1024;
fn hash(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
fn numbered(n: &str, prefix: &str) -> bool {
    n.strip_prefix(prefix).is_some_and(|n| {
        n.len() == 4
            && n.bytes().all(|b| b.is_ascii_digit())
            && n.parse::<u32>().is_ok_and(|n| (1..=4096).contains(&n))
    })
}
/// Paths one pass has found are not symlinks, nor any of their ancestors.
/// The run directories share every ancestor; a pass checks each once.
#[derive(Default)]
pub(super) struct Checked(BTreeSet<PathBuf>);

pub(super) fn directory(p: &Path) -> Result<()> {
    directory_in(p, &mut Checked::default())
}
fn directory_in(p: &Path, checked: &mut Checked) -> Result<()> {
    ensure!(p.is_absolute(), "managed evidence path must be absolute");
    let mut unchecked = Vec::new();
    for a in p.ancestors() {
        if checked.0.contains(a) {
            break;
        }
        ensure!(
            !fs::symlink_metadata(a)?.file_type().is_symlink(),
            "managed evidence symlink ancestor"
        );
        unchecked.push(a);
    }
    let m = fs::symlink_metadata(p)?;
    ensure!(
        m.is_dir() && m.permissions().mode() & 0o777 == 0o700,
        "managed evidence directory is not private"
    );
    checked
        .0
        .extend(unchecked.into_iter().map(Path::to_path_buf));
    Ok(())
}
fn names(p: &Path, remaining: &mut usize, checked: &mut Checked) -> Result<Vec<PathBuf>> {
    directory_in(p, checked)?;
    let mut out = Vec::new();
    for e in fs::read_dir(p)? {
        ensure!(*remaining > 0, "managed evidence entry bound exceeded");
        *remaining -= 1;
        out.push(e?.path());
    }
    out.sort();
    Ok(out)
}
pub(super) struct Held {
    pub(super) path: PathBuf,
    pub(super) bytes: Vec<u8>,
}
impl Held {
    pub(super) fn read(path: &Path, cap: u64) -> Result<Self> {
        Self::read_in(path, cap, &mut Checked::default())
    }
    fn read_in(path: &Path, cap: u64, checked: &mut Checked) -> Result<Self> {
        directory_in(
            path.parent().context("managed file parent absent")?,
            checked,
        )?;
        let before = fs::symlink_metadata(path)?;
        ensure!(
            before.is_file()
                && !before.file_type().is_symlink()
                && before.nlink() == 1
                && before.len() > 0
                && before.len() <= cap,
            "managed evidence file size/type/link bound"
        );
        let mut f = File::open(path)?;
        let same = |a: &fs::Metadata, b: &fs::Metadata| {
            a.dev() == b.dev()
                && a.ino() == b.ino()
                && a.len() == b.len()
                && a.mtime() == b.mtime()
                && a.mtime_nsec() == b.mtime_nsec()
                && a.ctime() == b.ctime()
                && a.ctime_nsec() == b.ctime_nsec()
        };
        ensure!(
            same(&before, &f.metadata()?),
            "managed evidence changed before open"
        );
        let mut bytes = Vec::new();
        (&mut f).take(cap + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == before.len()
                && same(&before, &f.metadata()?)
                && same(&before, &fs::symlink_metadata(path)?),
            "managed evidence changed during read"
        );
        Ok(Self {
            path: path.into(),
            bytes,
        })
    }
    fn recheck(&self, checked: &mut Checked) -> Result<()> {
        ensure!(
            Self::read_in(&self.path, self.bytes.len() as u64, checked)?.bytes == self.bytes,
            "managed evidence bytes changed"
        );
        Ok(())
    }
}

pub(crate) struct ManagedContext {
    pub(super) journal: Journal,
    pub(super) inputs: BTreeMap<[u8; 32], Vec<(PathBuf, usize)>>,
    held: Vec<Held>,
    held_bytes: u64,
    directories: Vec<(PathBuf, Vec<PathBuf>)>,
    pub(super) control: Control,
    /// Tracks an insert batch bound, read once per plan.
    batch_targets: Option<BTreeMap<u64, (InsertBatchId, usize)>>,
    /// Each batch's checked input (or why it is unusable), read once per plan.
    batch_inputs: BTreeMap<InsertBatchId, std::result::Result<Arc<BatchInput>, String>>,
    /// The source path each active binding records, read once per plan.
    recorded_sources: Option<BTreeMap<u64, PathBuf>>,
}
/// A batch's retained input, pinned by the journal: each target's absolute
/// source path (for an ALAC converted from a FLAC, the FLAC's, which the
/// journal records with the target) and the collection scope the batch was
/// planned from.
pub(super) struct BatchInput {
    pub(super) record: InsertBatchRecord,
    pub(super) sources: Vec<PathBuf>,
    pub(super) scope: String,
}
impl ManagedContext {
    /// The insert batch and target ordinal that bound this track, if any.
    pub(super) fn batch_target(&mut self, track: u64) -> Result<Option<(InsertBatchId, usize)>> {
        self.control.check()?;
        if self.batch_targets.is_none() {
            self.batch_targets = Some(self.journal.bound_insert_targets()?);
        }
        Ok(self
            .batch_targets
            .as_ref()
            .and_then(|targets| targets.get(&track).copied()))
    }
    /// The source path the journal records with this track's binding, if any.
    pub(super) fn recorded_source(&mut self, track: u64) -> Result<Option<PathBuf>> {
        self.control.check()?;
        if self.recorded_sources.is_none() {
            self.recorded_sources = Some(self.journal.recorded_sources()?);
        }
        Ok(self
            .recorded_sources
            .as_ref()
            .and_then(|sources| sources.get(&track).cloned()))
    }
    /// Check the journal, and every retained run file and directory inventory
    /// the context read; no new discovery is accepted here.
    pub(super) fn recheck(&self) -> Result<()> {
        self.control.check()?;
        self.journal.recheck_read_only_source()?;
        self.control.check()?;
        let mut remaining = MAX_ENTRIES * 4;
        let mut checked = Checked::default();
        for (path, expected) in &self.directories {
            ensure!(
                &names(path, &mut remaining, &mut checked)? == expected,
                "managed directory inventory changed"
            );
        }
        for held in &self.held {
            self.control.check()?;
            held.recheck(&mut checked)?;
        }
        self.control.check()
    }
}
