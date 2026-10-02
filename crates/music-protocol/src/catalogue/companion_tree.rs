//! Exact immediate companion observation. No producer authority.
use super::companion_metadata as metadata;
use anyhow::{Context, Result, ensure};
pub use metadata::{Attributes, Budget, Guard, Kind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub const ROOT: &str = "iTunes_Control/iTunes";
pub const GUARD_PATHS: [&str; 6] = [
    "iTunes_Control/iTunes/iTunesPrefs",
    "iTunes_Control/iTunes/iTunesPrefs.plist",
    "iTunes_Control/iTunes/iTunesCDB",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb",
    "iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal",
    "iTunes_Control/Artwork/ArtworkDB",
];
pub const MAX_GUARD_BYTES: u64 = 64 << 20;
pub const MAX_GUARD_AGGREGATE: u64 = 256 << 20;
pub const MAX_CANONICAL: usize = 512;
pub const MAX_MANIFEST_BYTES: usize = 256 << 10;
