//! One run per phone. A run locks each phone it reaches with a private file,
//! named as the phone's `devices` directory is, and keeps the lock until it
//! exits. Another run that reaches the same phone is refused at once: the
//! journal cannot tell a session a live run is still sending from one a killed
//! run left, and settling it, or sending beside it, would act on a session
//! still under way. The operating system drops the lock with the process, even
//! a killed one, so none is ever left behind.
use crate::engine::state::{current::hex, journal::device_binding, layout};

use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail, ensure};

/// The locks this process holds, until it exits.
static HELD: Mutex<Vec<(PathBuf, File)>> = Mutex::new(Vec::new());

/// Lock the phone `udid` in `directory` for the rest of this process, unless
/// it holds that lock already.
pub(super) fn take(directory: &Path, udid: &str) -> Result<()> {
    let path = path(directory, udid)?;
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    if held.iter().any(|(locked, _)| *locked == path) {
        return Ok(());
    }
    layout::private_directory(directory)?;
    let file = locked(&path)?;
    held.push((path, file));
    Ok(())
}

/// The lock of the phone `udid`: named by its device binding, never by the
/// identifier itself.
fn path(directory: &Path, udid: &str) -> Result<PathBuf> {
    Ok(directory.join(hex(&device_binding(udid)?)))
}

/// Lock the private file at `path`, created if missing, and record this
/// process in it; or refuse, naming the process that holds it.
fn locked(path: &Path) -> Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening the phone's lock {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "a phone's lock must be a private regular file (mode 0600): {}",
        path.display()
    );
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            // The holder may not have written its process yet.
            let mut holder = String::new();
            let _ = (&file).take(32).read_to_string(&mut holder);
            let process = holder
                .trim()
                .parse::<u32>()
                .map(|process| format!(" (process {process})"))
                .unwrap_or_default();
            bail!(
                "another rustunes run{process} is using this phone; wait for it to finish, then run this command again"
            );
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("locking {}", path.display()));
        }
    }
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(test)]
mod tests;
