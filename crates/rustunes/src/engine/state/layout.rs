//! Private managed engine state, and run directories kept while a journal
//! needs them: whole while their work is unfinished, then only the files
//! planning reads again.
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

pub fn private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o700,
            "state directory must be a private nonsymlink directory (mode 0700): {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            // Recheck after creation; never silently adopt an unsafe directory.
            private_directory(path)?;
            File::open(parent(path))?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn new_run(state: &Path) -> Result<PathBuf> {
    private_directory(state)?;
    let runs = state.join("runs");
    private_directory(&runs)?;
    let runs = fs::canonicalize(runs)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = runs.join(format!("{timestamp}-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    File::open(&runs)?.sync_all()?;
    Ok(path)
}

/// What `prune_runs` removed: whole runs, and the rest of the finished runs
/// it trimmed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub removed: usize,
    pub trimmed: usize,
    pub bytes: u64,
}

/// What planning reads again from a finished run: each `input.json` in it,
/// the `retained-source.m4a` a single insertion keeps, and the run's own
/// `run.json` and `plan.json`. An entry with one of these names is kept
/// whatever it is, so planning refuses a symlink or directory there as it
/// would have before.
const PLANNING_READS: [&str; 2] = ["input.json", "retained-source.m4a"];
const PLANNING_READS_AT_TOP: [&str; 2] = ["run.json", "plan.json"];

/// Remove the runs in `state` no journal needs, and trim the finished ones
/// to what planning reads again. A run stays whole while the process that
/// made it is running (this one included) and while a journal under
/// `devices` names it for unfinished work: a batch's or replacement's
/// evidence, or a single-track operation's input. A run a journal names only
/// for finished work is trimmed. A directory `new_run` did not name is left
/// alone, and a journal that cannot be read keeps every run.
pub fn prune_runs(state: &Path) -> Result<Pruned> {
    let runs = state.join("runs");
    let entries = match fs::read_dir(&runs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Pruned::default());
        }
        Err(error) => return Err(error.into()),
    };
    // Candidates first: a finished process adds nothing to a journal, and no
    // later process names a run a journal did not already name.
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(process) = name.to_str().and_then(run_process) else {
            continue;
        };
        if entry.file_type()?.is_dir() && !Path::new(&format!("/proc/{process}")).exists() {
            candidates.push(entry.path());
        }
    }
    if candidates.is_empty() {
        return Ok(Pruned::default());
    }
    let [unfinished, finished] = journal_references(state)?;
    let mut pruned = Pruned::default();
    for run in candidates {
        let inputs = inputs(&run)?;
        let names = |references: &References| {
            run.file_name()
                .is_some_and(|name| references.runs.contains(name))
                || !references.inputs.is_disjoint(&inputs)
        };
        if names(&unfinished) {
            continue;
        }
        if names(&finished) {
            let mut removed = false;
            trim(&run, true, &mut |bytes| {
                removed = true;
                pruned.bytes += bytes;
            })?;
            pruned.trimmed += usize::from(removed);
            continue;
        }
        let mut bytes = 0;
        files(&run, &mut |_, metadata| {
            bytes += metadata.len();
            Ok(())
        })?;
        fs::remove_dir_all(&run).with_context(|| format!("removing {}", run.display()))?;
        pruned.removed += 1;
        pruned.bytes += bytes;
    }
    if pruned != Pruned::default() {
        File::open(&runs)?.sync_all()?;
    }
    Ok(pruned)
}

/// Remove everything under `directory`, a finished run or a directory in
/// one, that planning does not read again, and the directories that leaves
/// empty, telling `removed` each entry's bytes. Whether it is now empty.
fn trim(directory: &Path, top: bool, removed: &mut impl FnMut(u64)) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let (path, metadata) = (entry.path(), entry.metadata()?);
        let name = entry.file_name();
        let read = |names: &[&str]| names.iter().any(|n| name == OsStr::new(n));
        if read(&PLANNING_READS) || top && read(&PLANNING_READS_AT_TOP) {
            empty = false;
        } else if metadata.is_dir() {
            if trim(&path, false, removed)? {
                fs::remove_dir(&path).with_context(|| format!("removing {}", path.display()))?;
                removed(0);
            } else {
                empty = false;
            }
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            removed(metadata.len());
        }
    }
    Ok(empty)
}

/// The process a run's name records, when `new_run` named it.
fn run_process(name: &str) -> Option<u32> {
    let (timestamp, process) = name.split_once('-')?;
    [timestamp, process]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .then(|| process.parse().ok())?
}

/// What the journals under `state/devices` name: the runs they give as
/// evidence and the input receipts they hold.
#[derive(Default)]
struct References {
    runs: BTreeSet<OsString>,
    inputs: BTreeSet<[u8; 32]>,
}

/// What the journals name for unfinished work, then for finished work.
fn journal_references(state: &Path) -> Result<[References; 2]> {
    let mut journals = Vec::new();
    match fs::read_dir(state.join("devices")) {
        Ok(devices) => {
            // `devices/<device>/<library>/` holds `journal` and the
            // `journal.previous` and `journal.pulling` a pull leaves.
            for device in devices {
                for library in fs::read_dir(device?.path())? {
                    for journal in fs::read_dir(library?.path())? {
                        let journal = journal?.path();
                        if journal.join("journal.sqlite").try_exists()? {
                            journals.push(journal);
                        }
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut references = [References::default(), References::default()];
    if journals.is_empty() {
        return Ok(references);
    }
    let scratch = std::env::temp_dir().join(format!(
        "rustunes-runs-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    let read = journals.iter().enumerate().try_for_each(|(n, journal)| {
        let named =
            crate::engine::state::journal::run_references(journal, &scratch.join(n.to_string()))
                .with_context(|| format!("reading {}", journal.display()))?;
        for (named, references) in [named.unfinished, named.finished]
            .into_iter()
            .zip(&mut references)
        {
            for evidence in named.evidence {
                // Evidence lies in `runs/<run>/`, or is the run itself.
                let parts: Vec<_> = evidence.components().map(|c| c.as_os_str()).collect();
                for pair in parts.windows(2) {
                    if pair[0] == "runs" && pair[1].to_str().and_then(run_process).is_some() {
                        references.runs.insert(pair[1].to_owned());
                    }
                }
            }
            references.inputs.extend(named.inputs);
        }
        anyhow::Ok(())
    });
    fs::remove_dir_all(&scratch)?;
    read?;
    Ok(references)
}

/// The SHA-256 of each `input.json` in `run`.
fn inputs(run: &Path) -> Result<BTreeSet<[u8; 32]>> {
    let mut inputs = BTreeSet::new();
    files(run, &mut |path, metadata| {
        if metadata.is_file() && path.file_name() == Some(OsStr::new("input.json")) {
            let mut digest = Sha256::new();
            std::io::copy(&mut File::open(path)?, &mut digest)?;
            inputs.insert(digest.finalize().into());
        }
        Ok(())
    })?;
    Ok(inputs)
}

/// Each entry under `directory` that is not a directory; symlinks are not
/// followed.
fn files(
    directory: &Path,
    each: &mut impl FnMut(&Path, &fs::Metadata) -> Result<()>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            files(&entry.path(), each)?;
        } else {
            each(&entry.path(), &metadata)?;
        }
    }
    Ok(())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_shared_directory_and_symlink_are_refused() {
        let root = std::env::temp_dir().join(format!(
            "music-state-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        private_directory(&root).unwrap();
        let shared = root.join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_directory(&shared).is_err());
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&root, &linked).unwrap();
        assert!(private_directory(&linked).is_err());
        let run = new_run(&root).unwrap();
        assert_eq!(
            fs::metadata(run).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runs_no_journal_needs_are_removed_and_finished_ones_trimmed() {
        use crate::engine::state::journal::{Journal, test_support};
        let state = std::env::temp_dir().join(format!(
            "music-state-prune-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let runs = state.join("runs");
        // No process has an ID above the kernel's 2^22 limit.
        let finished = |n: u32| runs.join(format!("{n}-99999999"));
        let file = |path: PathBuf, bytes: &[u8]| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        };
        let library = state.join("devices/d/l");
        fs::create_dir_all(&library).unwrap();
        assert_eq!(prune_runs(&state).unwrap(), Pruned::default());

        let (unfinished_input, finished_input) = (br#"{"n":2}"#, br#"{"n":8}"#);
        file(finished(1).join("audio/batch-001/input.json"), b"{}");
        file(
            finished(1).join("snapshot/catalog/MediaLibrary.sqlitedb"),
            b"0",
        );
        file(
            finished(2).join("audio/step-0001/input.json"),
            unfinished_input,
        );
        file(
            finished(2).join("audio/step-0001/before/catalog/MediaLibrary.sqlitedb"),
            b"0",
        );
        file(
            finished(3).join("snapshot/catalog/MediaLibrary.sqlitedb"),
            b"0123456789",
        );
        file(
            runs.join(format!("4-{}", std::process::id()))
                .join("plan.json"),
            b"{}",
        );
        file(runs.join("verify-5/plan.json"), b"{}");
        for (path, bytes) in [
            ("run.json", &b"{}"[..]),
            ("plan.json", b"{}"),
            ("snapshot/catalog/MediaLibrary.sqlitedb", b"01234"),
            ("audio/batch-001/input.json", b"{}"),
            ("audio/batch-001/plan.json", b"012"),
            ("audio/batch-001/sql/database.sqlite", b"01"),
        ] {
            file(finished(7).join(path), bytes);
        }
        file(
            finished(8).join("audio/step-0001/input.json"),
            finished_input,
        );
        file(
            finished(8).join("audio/step-0001/retained-source.m4a"),
            b"0",
        );
        // Planning refuses a symlink where it reads; trimming leaves it.
        fs::create_dir_all(finished(8).join("cover-0001")).unwrap();
        std::os::unix::fs::symlink(
            "../audio/step-0001/input.json",
            finished(8).join("cover-0001/input.json"),
        )
        .unwrap();
        file(
            finished(8).join("audio/step-0001/before/catalog/MediaLibrary.sqlitedb"),
            b"0123",
        );
        let mut journal =
            Journal::create(&library.join("journal"), &test_support::identity()).unwrap();
        let cancelled = test_support::prepare_insert_batch_at(
            &mut journal,
            test_support::alac_content(3),
            &finished(7).join("audio/batch-001"),
        );
        journal.cancel_insert_batch(cancelled).unwrap();
        test_support::prepare_insert_batch_at(
            &mut journal,
            test_support::alac_content(4),
            &finished(1).join("audio/batch-001"),
        );
        drop(journal);
        // A pull's previous journal is read too.
        let mut previous =
            Journal::create(&library.join("journal.previous"), &test_support::identity()).unwrap();
        let cancelled = test_support::prepare_insert_with_input(
            &mut previous,
            test_support::alac_content(6),
            Sha256::digest(finished_input).into(),
        );
        previous.cancel_prepared(cancelled).unwrap();
        test_support::prepare_insert_with_input(
            &mut previous,
            test_support::alac_content(5),
            Sha256::digest(unfinished_input).into(),
        );
        drop(previous);

        let pruned = Pruned {
            removed: 1,
            trimmed: 2,
            bytes: 10 + 5 + 3 + 2 + 4,
        };
        assert_eq!(prune_runs(&state).unwrap(), pruned);
        assert!(!finished(3).exists());
        // Unfinished work and a running process keep their whole run.
        for kept in [
            finished(1).join("snapshot/catalog/MediaLibrary.sqlitedb"),
            finished(2).join("audio/step-0001/before/catalog/MediaLibrary.sqlitedb"),
            runs.join(format!("4-{}", std::process::id())),
            runs.join("verify-5"),
        ] {
            assert!(kept.exists(), "{}", kept.display());
        }
        let left = |run: PathBuf| {
            let mut left = Vec::new();
            files(&run, &mut |path, _| {
                left.push(path.strip_prefix(&run).unwrap().to_owned());
                Ok(())
            })
            .unwrap();
            left.sort();
            left
        };
        let paths = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            left(finished(7)),
            paths(&["audio/batch-001/input.json", "plan.json", "run.json"])
        );
        assert!(!finished(7).join("snapshot").exists());
        assert_eq!(
            left(finished(8)),
            paths(&[
                "audio/step-0001/input.json",
                "audio/step-0001/retained-source.m4a",
                "cover-0001/input.json"
            ])
        );
        // What is left is trimmed already.
        assert_eq!(prune_runs(&state).unwrap(), Pruned::default());

        // A journal that cannot be read keeps every run.
        file(finished(6).join("plan.json"), b"{}");
        file(
            state.join("devices/d/other/journal/journal.sqlite"),
            b"not a journal",
        );
        assert!(prune_runs(&state).is_err());
        assert!(finished(6).exists());
        fs::remove_dir_all(state).unwrap();
    }
}
