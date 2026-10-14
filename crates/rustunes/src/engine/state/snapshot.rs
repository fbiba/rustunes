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
}

#[cfg(test)]
mod controlled_open_tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
}
