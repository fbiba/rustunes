//! One run per phone. A run locks each phone it reaches with a private file,
//! named as the phone's `devices` directory is, and keeps the lock until it
//! exits. Another run that reaches the same phone is refused at once: the
//! journal cannot tell a session a live run is still sending from one a killed
//! run left, and settling it, or sending beside it, would act on a session
//! still under way. The operating system drops the lock with the process, even
//! a killed one, so none is ever left behind.

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
