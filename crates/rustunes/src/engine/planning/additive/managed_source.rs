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
    /// One deadline for the whole plan: a full library with new tracks reads
    /// managed history for every row, which took over 120 s for a 399-source
    /// library.
    pub(crate) fn open(journal: &Journal, identity: &Identity) -> Result<Self> {
        let control = Control {
            deadline: tokio::time::Instant::now() + Duration::from_secs(1800),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        Self::open_with_control(journal, identity, &control)
    }
    /// The caller's phase deadline and Arc are retained without renewal.
    pub(crate) fn open_with_control(
        journal: &Journal,
        identity: &Identity,
        caller: &Control,
    ) -> Result<Self> {
        caller.check()?;
        let state = ancestry::managed_state(journal, identity)?;
        let devices = state.join("devices");
        let device = devices.join(hex(&identity.device_digest()));
        let library = device.join(hex(&identity.library_digest()));
        let path = library.join("journal");
        directory(&state)?;
        directory(&devices)?;
        directory(&device)?;
        directory(&library)?;
        let control = Control {
            deadline: caller.deadline,
            cancelled: caller.cancelled.clone(),
        };
        let mut this = Self {
            journal: Journal::open_read_only(&path, identity)?,
            inputs: BTreeMap::new(),
            held: Vec::new(),
            held_bytes: 0,
            directories: Vec::new(),
            batch_targets: None,
            batch_inputs: BTreeMap::new(),
            recorded_sources: None,
            control,
        };
        let runs = state.join("runs");
        let mut remaining = MAX_ENTRIES;
        let mut checked = Checked::default();
        let run_dirs = names(&runs, &mut remaining, &mut checked)?;
        ensure!(run_dirs.len() <= MAX_RUNS, "managed run bound exceeded");
        this.directories.push((runs, run_dirs.clone()));
        let mut total = 0u64;
        // Each directory is listed once: a run is also its own first root,
        // and a root its own first step.
        for run in run_dirs {
            this.control.check()?;
            let entries = names(&run, &mut remaining, &mut checked)?;
            this.directories.push((run.clone(), entries.clone()));
            if run.join("run.json").try_exists()? {
                this.retain(Held::read(&run.join("run.json"), MAX_INPUT)?)?;
            }
            let mut roots = vec![(run.clone(), entries.clone())];
            for p in entries {
                if p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n == "audio" || numbered(n, "cover-"))
                {
                    let listed = names(&p, &mut remaining, &mut checked)?;
                    this.directories.push((p.clone(), listed.clone()));
                    roots.push((p, listed));
                }
            }
            for (root, entries) in roots {
                let mut steps = vec![root];
                for p in entries {
                    if p.file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|n| numbered(n, "step-"))
                    {
                        let listed = names(&p, &mut remaining, &mut checked)?;
                        this.directories.push((p.clone(), listed));
                        steps.push(p);
                    }
                }
                for step in steps {
                    this.control.check()?;
                    if step.join("input.json").try_exists()? {
                        let held = Held::read(&step.join("input.json"), MAX_INPUT)?;
                        total = total
                            .checked_add(held.bytes.len() as u64)
                            .context("managed input size overflow")?;
                        ensure!(
                            total <= MAX_INPUTS_TOTAL,
                            "managed input aggregate bound exceeded"
                        );
                        let index = this.held.len();
                        this.inputs
                            .entry(hash(&held.bytes))
                            .or_default()
                            .push((run.clone(), index));
                        this.retain(held)?;
                    }
                }
            }
        }
        this.recheck()?;
        Ok(this)
    }
    pub(super) fn retain(&mut self, held: Held) -> Result<()> {
        if let Some(previous) = self.held.iter().find(|p| p.path == held.path) {
            ensure!(
                previous.bytes == held.bytes,
                "managed retained evidence changed"
            );
            return Ok(());
        }
        self.held_bytes = self
            .held_bytes
            .checked_add(held.bytes.len() as u64)
            .context("managed evidence size overflow")?;
        ensure!(
            self.held_bytes <= MAX_INPUTS_TOTAL,
            "managed held evidence aggregate bound exceeded"
        );
        self.held.push(held);
        Ok(())
    }
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
    /// A batch's retained input, checked against the journal once per plan.
    pub(super) fn batch_input(&mut self, id: InsertBatchId) -> Result<Arc<BatchInput>> {
        if !self.batch_inputs.contains_key(&id) {
            let input = self
                .read_batch_input(id)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"));
            self.batch_inputs.insert(id, input);
        }
        self.batch_inputs[&id]
            .clone()
            .map_err(|error| anyhow!(error))
    }
    fn read_batch_input(&mut self, id: InsertBatchId) -> Result<BatchInput> {
        self.control.check()?;
        let record = self.journal.insert_batch(id)?;
        let held = Held::read(&record.evidence.join("input.json"), MAX_INPUT)?;
        ensure!(
            hash(&held.bytes) == record.input_receipt,
            "retained batch input differs from the journal"
        );
        let input: Value = serde_json::from_slice(&held.bytes)?;
        // A session's input adds the tracks it removed; its members are an
        // insert batch's.
        ensure!(
            input["schema"] == "rustunes.insert-batch-input"
                || input["schema"] == "rustunes.session-input",
            "unsupported batch input schema"
        );
        let members = input["members"]
            .as_array()
            .context("batch input has no members")?;
        ensure!(
            members.len() == record.targets.len(),
            "batch input and journal targets differ"
        );
        let mut sources = Vec::with_capacity(members.len());
        for (member, target) in members.iter().zip(&record.targets) {
            let content = &target.target.content;
            ensure!(
                member["track"] == format!("{:016x}", content.track_pid)
                    && member["path"] == content.path.as_str()
                    && member["full_source_sha256"] == hex(&content.source.full_file_sha256),
                "batch input member differs from its journal target"
            );
            let source = PathBuf::from(
                member["source_path"]
                    .as_str()
                    .context("batch member has no source path")?,
            );
            ensure!(source.is_absolute(), "batch member source path is relative");
            // The converted file was temporary; the FLAC is the source.
            sources.push(match &target.target.conversion {
                Some(conversion) => conversion.source_path.clone(),
                None => source,
            });
        }
        let scope = input["collection_scope_sha256"]
            .as_str()
            .context("batch input has no collection scope")?
            .to_owned();
        self.retain(held)?;
        Ok(BatchInput {
            record,
            sources,
            scope,
        })
    }
    pub(super) fn find(&mut self, digest: [u8; 32]) -> Result<(PathBuf, Vec<u8>)> {
        self.control.check()?;
        let rows = self
            .inputs
            .get(&digest)
            .context("missing managed producer input digest")?;
        ensure!(rows.len() == 1, "ambiguous managed producer input digest");
        let (run, index) = rows[0].clone();
        let (step, before, bytes) = crate::engine::recovery::retained::locate_step(&run, digest)?;
        ensure!(
            bytes == self.held[index].bytes,
            "managed locator input differs"
        );
        let input: Value = serde_json::from_slice(&bytes)?;
        if input.get("pre_session_attempt").is_some() {
            let phase = before.parent().context("pre-session phase absent")?;
            let held = Held::read(&phase.join("input.json"), MAX_INPUT)?;
            ensure!(
                held.bytes == bytes,
                "pre-session locator phase input differs"
            );
            self.retain(held)?;
            if !self.directories.iter().any(|(p, _)| p == phase) {
                let used = self.directories.iter().map(|(_, n)| n.len()).sum::<usize>();
                let mut remaining = MAX_ENTRIES.saturating_sub(used);
                let inventory = names(phase, &mut remaining, &mut Checked::default())?;
                self.directories.push((phase.to_path_buf(), inventory));
            }
        }
        Ok((step, bytes))
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
