//! Load a complete private read-only capture for offline comparison/planning.
//! Loading saved evidence never establishes that today's phone is unchanged.
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::engine::state::audio_index::{Absent, DeviceAudioIndex, IndexIdentity};
use music_protocol::catalogue::identity;
use music_protocol::catalogue::sqlite::{self, Catalog};

pub struct Snapshot {
    /// Original captured SQL paths, independently hash-bound by catalog/index.
    pub catalog_database: PathBuf,
    pub catalog_wal: Option<PathBuf>,
    pub catalog: Catalog,
    pub index: DeviceAudioIndex,
    pub identity: IndexIdentity,
    pub index_receipt_sha256: [u8; 32],
    pub raw_preferences: Vec<u8>,
    pub plist_preferences: Vec<u8>,
}

pub fn open(directory: &Path) -> Result<Snapshot> {
    open_inner(directory, None, Absent::NONE)
}

/// `open` for a capture that allowed rows without audio (`Absent`): rows an
/// interrupted insertion left (its own targets) are accepted when the capture
/// recorded exactly those, and, when allowed, rows whose file is gone.
/// `index.unfilled()` and `index.gone()` name them.
pub(crate) fn open_allowing(directory: &Path, absent: Absent<'_>) -> Result<Snapshot> {
    open_inner(directory, None, absent)
}

/// Same saved-evidence validation under the caller's original clock, with
/// memory-only SQLite temporary storage and unchanged historical input caps.
pub(crate) fn open_with_control(
    directory: &Path,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<Snapshot> {
    open_allowing_with_control(directory, Absent::NONE, control)
}

/// `open_allowing` under the caller's clock, as `open_with_control`.
pub(crate) fn open_allowing_with_control(
    directory: &Path,
    absent: Absent<'_>,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<Snapshot> {
    control.check()?;
    let result = open_inner(directory, Some(control), absent)?;
    control.check()?;
    Ok(result)
}

fn open_inner(
    directory: &Path,
    control: Option<&crate::engine::execution::native_insert::Control>,
    absent: Absent<'_>,
) -> Result<Snapshot> {
    private_directory(directory)?;
    private_directory(&directory.join("catalog"))?;
    let receipt_path = directory.join("complete-index.json");
    let bytes = read_stable(&receipt_path, 128 * 1024 * 1024).context(
        "a complete device snapshot is required; incomplete progress cannot prove absence",
    )?;
    let receipt_sha: [u8; 32] = Sha256::digest(&bytes).into();
    let descriptor: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        descriptor["complete"] == true,
        "device snapshot is incomplete"
    );
    let identity = IndexIdentity {
        device_identifier_sha256: digest(&descriptor["identity"]["device_identifier_sha256"])?,
        library_identity_sha256: digest(&descriptor["identity"]["library_identity_sha256"])?,
    };
    let before_raw = read_stable(&directory.join("before-iTunesPrefs"), 1024 * 1024)?;
    let before_plist = read_stable(&directory.join("before-iTunesPrefs.plist"), 1024 * 1024)?;
    let raw_preferences = read_stable(&directory.join("after-iTunesPrefs"), 1024 * 1024)?;
    let plist_preferences = read_stable(&directory.join("after-iTunesPrefs.plist"), 1024 * 1024)?;
    let library = identity::parse(&before_raw)?;
    for prefs in [&before_plist, &raw_preferences, &plist_preferences] {
        library.ensure_matches(&identity::parse(prefs)?)?;
    }
    ensure!(
        library.binding_sha256() == identity.library_identity_sha256,
        "snapshot preferences and index library bindings differ"
    );
    let progress = read_stable(&directory.join("progress.jsonl"), 128 * 1024 * 1024)?;
    ensure!(
        <[u8; 32]>::from(Sha256::digest(&progress)) == digest(&descriptor["progress_sha256"])?,
        "snapshot completion and durable progress hashes differ"
    );
    let wal = directory.join("catalog/MediaLibrary.sqlitedb-wal");
    let has_wal = !descriptor["catalog"]["wal_sha256"].is_null();
    ensure!(
        has_wal == wal.try_exists()?,
        "snapshot WAL presence differs from index"
    );
    let database = directory.join("catalog/MediaLibrary.sqlitedb");
    let catalog = match control {
        Some(control) => sqlite::read_snapshot_memory_with_control(
            &database,
            has_wal.then_some(wal.as_path()),
            &std::env::temp_dir(),
            control.deadline.into_std(),
            control.cancelled.clone(),
        )?,
        None => sqlite::read_snapshot(
            &database,
            has_wal.then_some(wal.as_path()),
            &std::env::temp_dir(),
        )?,
    };
    let index = DeviceAudioIndex::load_complete_allowing(
        &receipt_path,
        receipt_sha,
        &catalog,
        &identity,
        absent,
    )?;
    Ok(Snapshot {
        catalog_database: directory.join("catalog/MediaLibrary.sqlitedb"),
        catalog_wal: has_wal.then_some(wal),
        catalog,
        index,
        identity,
        index_receipt_sha256: receipt_sha,
        raw_preferences,
        plist_preferences,
    })
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o777 == 0o700,
        "snapshot directories must be nonsymlink private directories (mode 0700)"
    );
    Ok(())
}
fn read_stable(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && before.len() <= maximum && before.permissions().mode() & 0o077 == 0,
        "snapshot input must be a bounded private regular file"
    );
    let mut file = File::open(path)?;
    ensure!(
        same(&before, &file.metadata()?),
        "snapshot file changed while opening"
    );
    let mut bytes = Vec::new();
    (&mut file).take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len()
            && same(&before, &file.metadata()?)
            && same(&before, &fs::symlink_metadata(path)?),
        "snapshot input changed while reading"
    );
    Ok(bytes)
}
fn same(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    after.is_file()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}
fn digest(value: &Value) -> Result<[u8; 32]> {
    let text = value.as_str().context("snapshot hash is missing")?;
    ensure!(
        text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "snapshot hash must be canonical SHA256"
    );
    let mut hash = [0; 32];
    for (i, part) in text.as_bytes().chunks_exact(2).enumerate() {
        hash[i] = u8::from_str_radix(std::str::from_utf8(part)?, 16)?;
    }
    ensure!(hash != [0; 32], "missing snapshot evidence hash");
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_completion_and_noncanonical_hashes() {
        assert!(open(Path::new("/tmp/nonexistent-rustunes-snapshot")).is_err());
        for text in ["", "abc", &"G".repeat(64), &"0".repeat(64)] {
            assert!(digest(&Value::String(text.into())).is_err());
        }
        assert_eq!(digest(&Value::String("ab".repeat(32))).unwrap(), [0xab; 32]);
    }
}

#[cfg(test)]
mod controlled_open_tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    #[test]
    fn saved_snapshot_original_cancellation_refuses_before_filesystem_access() {
        let control = crate::engine::execution::native_insert::Control {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            cancelled: Arc::new(AtomicBool::new(true)),
        };
        let error = open_with_control(
            Path::new("/nonexistent-controlled-saved-snapshot"),
            &control,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cancel"), "{error:#}");
    }
    #[test]
    fn saved_snapshot_expired_original_deadline_is_not_renewed() {
        let control = crate::engine::execution::native_insert::Control {
            deadline: tokio::time::Instant::now() - std::time::Duration::from_secs(1),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let error = open_with_control(
            Path::new("/nonexistent-controlled-saved-snapshot"),
            &control,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("deadline"), "{error:#}");
    }
}
