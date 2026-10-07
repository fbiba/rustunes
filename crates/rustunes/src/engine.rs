//! Typed synchronization entry points, independent of command-line parsing and
//! presentation. Requests describe intent; persisted attempts and native permit
//! types continue to control device writes and verified completion.
use anyhow::{Result, ensure};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::Instant;

mod events;
mod observation;
pub use observation::Observation;
pub(crate) mod planning;
pub(crate) mod state;
pub use crate::engine::planning::source_collection::SelectionOptions;
pub use events::Event;

type Observer = Arc<dyn for<'a> Fn(Event<'a>) -> Result<()> + Send + Sync>;
tokio::task_local! { static CURRENT_OBSERVER: Observer; }

/// Observe a nested engine mechanism without granting it command-line coupling.
pub(crate) fn observe(event: Event<'_>) -> Result<()> {
    CURRENT_OBSERVER
        .try_with(|observer| observer(event))
        .unwrap_or(Ok(()))
}

/// Report a warning the run goes on past to the observer; where none is
/// installed, print `fallback` on stderr instead.
pub(crate) fn warn(event: Event<'_>, fallback: impl FnOnce() -> String) {
    if CURRENT_OBSERVER
        .try_with(|observer| {
            let _ = observer(event);
        })
        .is_err()
    {
        eprintln!("{}", fallback());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyResult {
    Preflight {
        additions: usize,
    },
    Unchanged,
    Completed {
        additions: usize,
    },
    PreflightSourceChanges {
        additions: usize,
        metadata_updates: usize,
    },
}
impl ApplyResult {
    pub fn metadata_updates(self) -> usize {
        match self {
            Self::PreflightSourceChanges {
                metadata_updates, ..
            } => metadata_updates,
            _ => 0,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverResult {
    Unchanged,
    Planned {
        track_id: u64,
        cache_id: u32,
        bytes: usize,
    },
    Verified,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncOutcome {
    Blocked,
    Preflight,
    Completed,
}
#[derive(Debug)]
pub struct SyncResult {
    pub outcome: SyncOutcome,
    pub run: PathBuf,
    pub plan: PathBuf,
    pub journal: PathBuf,
    pub managed_covers_verified: usize,
    pub existing_unmanaged_covers_unverified: usize,
    pub metadata_updates_verified: usize,
    /// New covers add-only left out because their album shows another image.
    pub covers_left_out: usize,
    /// The sources the sync left out, or a dry run would leave out.
    pub skipped: Vec<Skipped>,
    /// Tracks the removal before the sync left it, which it removed
    /// (`SyncRequest::removals`).
    pub removed: usize,
}
impl SyncResult {
    pub fn ready(&self) -> bool {
        self.outcome != SyncOutcome::Blocked
    }
    /// Whether every source the plan kept was sent: none was dropped while
    /// it was being sent.
    pub fn sent_all(&self) -> bool {
        !self.skipped.iter().any(|s| s.stage == SkipStage::Send)
    }
    pub(crate) fn new(
        outcome: SyncOutcome,
        run: &Path,
        plan: &Path,
        journal: &Path,
        managed: usize,
        unmanaged: usize,
    ) -> Self {
        Self {
            outcome,
            run: run.into(),
            plan: plan.into(),
            journal: journal.into(),
            managed_covers_verified: managed,
            existing_unmanaged_covers_unverified: unmanaged,
            metadata_updates_verified: 0,
            covers_left_out: 0,
            skipped: Vec::new(),
            removed: 0,
        }
    }
}

/// A source a sync left out, and why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Skipped {
    /// Relative to the source root.
    pub path: String,
    pub code: &'static str,
    /// Why, in plain words, and what to do.
    pub reason: String,
    /// The refusal in full.
    pub detail: String,
    pub stage: SkipStage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipStage {
    /// The plan left it out (`additive::Decision::Skip`).
    Plan,
    /// It was dropped while being sent: its FLAC's conversion failed.
    Send,
}

/// What holds a sync before anything is sent.
pub struct SyncHolds {
    /// Each refusal that holds it.
    pub holds: Vec<Hold>,
    /// How to send the rest without the sources that hold it, when only
    /// sources do.
    pub hint: Option<String>,
}

#[derive(serde::Serialize)]
pub struct Hold {
    /// The source, relative to the source root; `None` for the journal.
    pub path: Option<String>,
    pub code: String,
    pub reason: String,
}

pub(crate) mod execution;

pub(crate) mod recovery;
