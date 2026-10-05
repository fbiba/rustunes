//! Private, bounded files for retained evidence, independent of workflow authority.
//!
//! These helpers preserve the retained-evidence policy: canonical mode 0700
//! directories, private regular files, bounded reads, stable descriptor/path
//! identity and durable create-new writes. They do not certify a journal,
//! ownership relationship or protocol outcome.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn digest(s: &str) -> Result<[u8; 32]> {
    ensure!(
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "noncanonical SHA256"
    );
    let mut out = [0; 32];
    for (i, p) in s.as_bytes().chunks_exact(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(p)?, 16)?;
    }
    ensure!(out != [0; 32], "zero evidence hash");
    Ok(out)
}
pub fn json_hash(v: &serde_json::Value, k: &str) -> Result<[u8; 32]> {
    digest(v[k].as_str().context("missing JSON hash")?)
}
pub fn directory(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.permissions().mode() & 0o777 == 0o700
            && path.is_absolute()
            && path.canonicalize()? == path,
        "evidence directory must be canonical nonsymlink mode 0700"
    );
    Ok(())
}
fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
pub fn read(path: &Path, cap: u64) -> Result<Vec<u8>> {
    directory(path.parent().context("missing input parent")?)?;
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.permissions().mode() & 0o077 == 0 && m.len() <= cap,
        "input must be bounded private regular file"
    );
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(same(&m, &f.metadata()?), "input changed during open");
    let mut b = Vec::new();
    (&mut f).take(cap + 1).read_to_end(&mut b)?;
    ensure!(
        b.len() as u64 == m.len()
            && same(&m, &f.metadata()?)
            && same(&m, &fs::symlink_metadata(path)?),
        "input changed during read"
    );
    Ok(b)
}
pub fn file_hash(path: &Path, cap: u64) -> Result<(u64, [u8; 32])> {
    file_hash_checked(path, cap, None)
}
pub fn file_hash_with_control(
    path: &Path,
    cap: u64,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<(u64, [u8; 32])> {
    file_hash_checked(path, cap, Some((deadline, &cancelled)))
}
fn check_hash_control(control: Option<(Instant, &AtomicBool)>) -> Result<()> {
    if let Some((deadline, cancelled)) = control {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "retained hash cancelled"
        );
        ensure!(Instant::now() < deadline, "retained hash deadline exceeded");
    }
    Ok(())
}
fn file_hash_checked(
    path: &Path,
    cap: u64,
    control: Option<(Instant, &AtomicBool)>,
) -> Result<(u64, [u8; 32])> {
    check_hash_control(control)?;
    directory(path.parent().context("missing input parent")?)?;
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.permissions().mode() & 0o077 == 0 && m.len() <= cap,
        "input must be bounded private regular file"
    );
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(same(&m, &f.metadata()?), "input changed during open");
    check_hash_control(control)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_hash_control(control)?;
        let n = f.read(&mut buffer)?;
        check_hash_control(control)?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n as u64)
            .context("file length overflow")?;
        ensure!(count <= cap, "file exceeds bound");
        hash.update(&buffer[..n]);
    }
    ensure!(
        count == m.len() && same(&m, &f.metadata()?) && same(&m, &fs::symlink_metadata(path)?),
        "input changed during read"
    );
    check_hash_control(control)?;
    Ok((count, hash.finalize().into()))
}
#[cfg(test)]
pub fn make_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    directory(path.parent().context("missing output parent")?)?;
    fs::DirBuilder::new().mode(0o700).create(path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn write(path: &Path, b: &[u8]) -> Result<()> {
    directory(path.parent().context("missing output parent")?)?;
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(b)?;
    f.sync_all()?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests;
