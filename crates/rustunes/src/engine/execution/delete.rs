//! Frozen removal selection, removed in verified batches (one native session
//! each, see `removal_batch`). Explicit deletion selects named tracks; prune
//! uses full-source ownership rules; sync and `plan --remove-missing` select
//! the tracks this tool inserted whose source file is gone from the selection, by
//! the source path the journal pins; Clear selects every track under one
//! coordination that closes only when the phone's catalogue is empty: once no
//! track is left, it deletes every ordinary playlist in a last batch. Smart,
//! folder and phone-made playlists stay.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    num::NonZeroU64,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, atomic::Ordering},
    time::SystemTime,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use crate::{
    engine::execution::device,
    engine::planning::additive::JournalEvidence,
    engine::planning::removal::{self, Decision, Origin, Request},
    engine::planning::source_collection::{self, Selection, SelectionOptions, SourceCollection},
    engine::state::audio_index::Absent,
    engine::state::journal::{
        BatchState, CoordinationId, CoordinationKind, Identity, Journal, PlaylistTarget,
    },
    engine::state::layout,
    engine::state::retained_files as io,
    engine::state::snapshot,
    engine::verification::audio_capture,
    engine::{
        execution::{insert_batch, removal_batch},
        planning::change::MAX_BATCH,
        verification::removal_sql,
    },
};
use music_protocol::catalogue::mutation;
use music_protocol::protocol::trace::private_write;

pub struct Options {
    /// Save an exact removal proposal without changing the phone or journal.
    pub dry_run: bool,
    /// Reuse a complete snapshot; execution rechecks current device state.
    pub snapshot: Option<PathBuf>,
    /// Existing per-device journal. Defaults to this tool's private state.
    pub journal: Option<PathBuf>,
    pub state_dir: PathBuf,
    /// Also save the full selection and disclosed side effects to this new file.
    pub output: Option<PathBuf>,
    pub host_name: String,
    /// Maximum seconds for each complete audio verification pass.
    pub seconds: u64,
    /// Tracks removed per native session.
    pub batch_size: usize,
    /// A sync follows (`Missing`): the removals that do not fill a batch are
    /// left to it (`Outcome::Completed::left`), and its first insert batch
    /// removes them in its session before its new tracks, as Apple's host
    /// sends a sync. They are all sent here when an ordinary playlist holds
    /// one: such a session removes no playlist member.
    pub leave_for_sync: bool,
    /// A saved plan's removals (`apply`): the selection must be exactly its
    /// tracks, or nothing is sent.
    pub pinned: Option<crate::engine::ChangePins>,
}

/// A nonzero catalogue track identifier, parsed from exactly 16 ASCII hex digits.
/// Parsing accepts either letter case; display uses 16 lowercase digits.
/// This identifies a track and grants no deletion authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(NonZeroU64);

impl TrackId {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// A malformed or zero catalogue track identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseTrackIdError {
    InvalidFormat,
    Zero,
}

impl fmt::Display for ParseTrackIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFormat => "track IDs require exactly 16 hexadecimal digits",
            Self::Zero => "zero or duplicate track ID",
        })
    }
}
impl std::error::Error for ParseTrackIdError {}

impl TryFrom<u64> for TrackId {
    type Error = ParseTrackIdError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(ParseTrackIdError::Zero)
    }
}

impl FromStr for TrackId {
    type Err = ParseTrackIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 16 || !value.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(ParseTrackIdError::InvalidFormat);
        }
        u64::from_str_radix(value, 16)
            .map_err(|_| ParseTrackIdError::InvalidFormat)?
            .try_into()
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.get())
    }
}

/// Supported removal requests.
pub enum Mode {
    /// Select validated nonzero track IDs; empty or duplicate selections fail.
    Delete(Vec<TrackId>),
    /// Remove owned tracks absent from this source directory, with its
    /// subfolders when `recursive`.
    Prune { source: PathBuf, recursive: bool },
    /// Remove the tracks this tool inserted whose source file is gone from
    /// this directory's selection (sync and `plan --remove-missing`).
    Missing {
        source: PathBuf,
        selection: SelectionOptions,
    },
    /// Remove every track. An interrupted Clear continues under its Active
    /// coordination when run again.
    Clear,
}

/// Frontends choose their own exit status from the supported removal outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Blocked,
    /// `sync`'s removals of `tracks` wait: a source this run cannot read may
    /// be one of theirs, moved. Nothing was removed.
    Held {
        tracks: usize,
    },
    Cancelled,
    Preview,
    Completed {
        removed: usize,
        deleted_playlists: usize,
        /// Tracks left to the sync that follows (`Options::leave_for_sync`).
        left: Vec<u64>,
    },
}
impl Outcome {
    pub fn ready(&self) -> bool {
        matches!(self, Self::Preview | Self::Completed { .. })
    }
}

/// A confirmation is requested only for an immutable, fully supported selection.
/// It never substitutes for per-operation qualification or a Started permit.
pub struct Confirmation<'a> {
    pub count: usize,
    /// Ordinary playlists a Clear deletes once its tracks are gone.
    pub playlists: usize,
    pub selection: &'a Path,
}
pub enum Event<'a> {
    InspectingSources {
        done: usize,
        total: usize,
    },
    Evidence(&'a Path),
    Selection {
        descriptor: &'a serde_json::Value,
        path: &'a Path,
    },
    Blocked,
    /// Every removal of `tracks` waits for these unreadable `sources`.
    Held {
        tracks: usize,
        sources: &'a [String],
    },
    DryRunComplete,
    Cancelled,
    Step {
        number: usize,
        total: usize,
    },
    /// An earlier run's unfinished removal batch, settled before selecting.
    Settled {
        batch: i64,
        state: &'static str,
    },
    Completed {
        removed: usize,
        deleted_playlists: usize,
        /// Removals left to the sync that follows.
        left: usize,
        evidence: Option<&'a Path>,
    },
}

/// The removals a sync's removal leaves it: those that do not fill a batch,
/// which its first insert batch removes in its session before its new
/// tracks. None when `alone` says they go by themselves: an ordinary playlist
/// holds one, which such a session does not remove, or one's file is already
/// gone, which the sync's capture refuses.
fn left_for_sync(
    selected: &[u64],
    batch_size: usize,
    alone: impl FnOnce(&[u64]) -> Result<bool>,
) -> Result<&[u64]> {
    let tail = &selected[selected.len() / batch_size * batch_size..];
    Ok(match !tail.is_empty() && alone(tail)? {
        true => &[],
        false => tail,
    })
}

/// Read every selected source, reporting progress.
fn inspect(selection: &Selection, engine: &crate::engine::Engine) -> Result<SourceCollection> {
    source_collection::inspect_with_progress(selection, |done, total| {
        if done == 0 || done == total || done % 25 == 0 {
            engine.emit(crate::engine::Event::Deletion(Event::InspectingSources {
                done,
                total,
            }))?;
        }
        Ok(())
    })
}

/// The selected sources whose audio cannot be identified: those inspection
/// could not read, and MP3s whose frames do not check.
fn unreadable(sources: &source_collection::SourceCollection) -> Vec<String> {
    let mut paths: Vec<_> = sources
        .failures
        .iter()
        .map(|failed| failed.relative_path.clone())
        .chain(
            sources
                .mp3_tracks
                .iter()
                .filter(|source| source.source.transferred_identity().is_err())
                .map(|source| source.relative_path.clone()),
        )
        .collect();
    paths.sort();
    paths
}

/// Each track this tool inserted, against the selection: the source path the
/// journal pins is gone, still there, out of the selection's reach, or not
/// known.
pub(crate) fn missing_origins(
    journal: &Journal,
    identity: &Identity,
    selected: &Selection,
    control: &crate::engine::execution::native_insert::Control,
) -> Result<BTreeMap<u64, Origin>> {
    let paths = match crate::engine::planning::additive::source_paths(journal, identity, control) {
        Ok(paths) => paths,
        // Without the run history no source can be named, and every track stays.
        Err(error) => {
            let reason = format!("{error:#}");
            return Ok(journal
                .active_verified_bindings()?
                .into_iter()
                .map(|b| (b.content.track_pid, Origin::Unknown(reason.clone())))
                .collect());
        }
    };
    paths
        .into_iter()
        .map(|(track, path)| {
            let origin = match path {
                Err(reason) => Origin::Unknown(reason),
                Ok(path)
                    if !source_collection::reaches(selected.root(), selected.options(), &path) =>
                {
                    Origin::Outside
                }
                Ok(path) => match std::fs::symlink_metadata(&path) {
                    Ok(_) => Origin::Present,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Origin::Missing(path)
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("checking source {}", path.display()));
                    }
                },
            };
            Ok((track, origin))
        })
        .collect()
}
