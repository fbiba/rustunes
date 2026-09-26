//! Pin and verify exact local source bytes without granting execution authority.
//! The descriptor, pathname and original metadata remain bound for the lifetime
//! of the observation. This is independent of journals and operation permits.
use crate::{media::fingerprint::SourceFingerprint, protocol::transfer::MAX_AUDIO};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Exact stable regular-file identity used by source inspection and retention.
pub fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.is_file()
        && b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

/// Compare a retained filesystem observation using the original selection and
/// classification timestamp policy, including creation time where available.
/// This observation remains distinct from the stricter pinned-source check.
pub fn same_observed_file(left: &fs::Metadata, right: &fs::Metadata) -> Result<bool> {
    let same = left.file_type() == right.file_type()
        && left.len() == right.len()
        && left.modified()? == right.modified()?
        && left.created().ok() == right.created().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(same
            && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec())
    }
    #[cfg(not(unix))]
    {
        Ok(same)
    }
}

pub struct BoundSource {
    file: File,
    metadata: fs::Metadata,
    path: PathBuf,
}
impl BoundSource {
    /// The pinned descriptor used for bounded transfer. Callers must retain the
    /// surrounding check-before/check-after protocol when reading this handle.
    pub fn file(&mut self) -> &mut File {
        &mut self.file
    }
    /// Length bound retained when the source was opened and verified.
    pub fn file_bytes(&self) -> u64 {
        self.metadata.len()
    }
    /// Read the already pinned source through the same handle for durable
    /// recovery retention. Refuse substitutions or size changes during copying.
    pub fn read_for_retention(&mut self) -> Result<Vec<u8>> {
        self.check()?;
        self.file.rewind()?;
        let expected = self.metadata.len();
        let mut bytes = Vec::with_capacity(expected as usize);
        (&mut self.file)
            .take(expected + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == expected,
            "source size changed during retention"
        );
        self.check()?;
        Ok(bytes)
    }
    pub fn open(path: &Path, expected: &SourceFingerprint) -> Result<Self> {
        Self::open_observed(path, expected.file_bytes, expected.full_file_sha256)
    }
    /// Pin an exact retained container without assigning an ALAC identity.
    pub fn open_observed(path: &Path, bytes: u64, sha256: [u8; 32]) -> Result<Self> {
        ensure!(
            bytes > 0 && sha256 != [0; 32],
            "missing observed file evidence"
        );
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && metadata.len() == bytes && metadata.len() <= MAX_AUDIO,
            "source type/size differs from preparation"
        );
        Self::open_pinned(path, metadata, bytes, sha256)
    }
    fn open_pinned(
        path: &Path,
        metadata: fs::Metadata,
        bytes: u64,
        sha256: [u8; 32],
    ) -> Result<Self> {
        // The path may have changed since symlink_metadata. Do not follow a
        // replacement symlink or block opening a substituted FIFO before the
        // descriptor/path identity checks can reject it.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let mut source = Self {
            file,
            metadata,
            path: path.to_owned(),
        };
        source.check()?;
        let mut digest = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        let mut remaining = bytes;
        while remaining > 0 {
            let length = remaining.min(buffer.len() as u64) as usize;
            source
                .file
                .read_exact(&mut buffer[..length])
                .context("source shortened during validation")?;
            digest.update(&buffer[..length]);
            remaining -= length as u64;
        }
        ensure!(
            source.file.read(&mut buffer[..1])? == 0,
            "source grew during validation"
        );
        ensure!(
            <[u8; 32]>::from(digest.finalize()) == sha256,
            "source hash differs from prepared transferred container"
        );
        source.check()?;
        source.file.rewind()?;
        Ok(source)
    }
    pub fn check(&self) -> Result<()> {
        for now in [self.file.metadata()?, fs::symlink_metadata(&self.path)?] {
            ensure!(
                same_file(&self.metadata, &now),
                "source handle or path changed during insertion"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
