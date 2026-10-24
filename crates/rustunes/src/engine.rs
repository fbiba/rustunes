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
pub(crate) mod verification;
pub use crate::engine::execution::cover::Options as CoverRequest;
pub use crate::engine::execution::device::{Config as DeviceConfig, Event as DeviceEvent};
pub use crate::engine::execution::sync::Options as SyncRequest;
pub use crate::engine::planning::source_collection::SelectionOptions;
pub use crate::engine::verification::journal::{AudioStatus, Report as VerificationResult};
pub use events::Event;
pub use execution::pull::{
    NOT_IN_LIBRARY, PhoneTrack as PulledTrack, Report as PullResult, Request as PullRequest,
};
pub use execution::replace::{
    Kind as ReplaceKind, Outcome as ReplaceOutcome, Preview as ReplacePreview,
    Removal as ReplaceRemoval, Request as ReplaceRequest,
};
pub use planning::change::{
    Action as ChangeAction, ChangePlan, Pinned as ChangePins, Planned as ChangePlanned,
    Request as ChangePlanRequest,
};
pub use planning::metadata_intent::{
    MetadataIntent, MetadataPlan, MetadataPlanRequest, MetadataPlanResult, MetadataRequestDocument,
    MetadataWireCandidate,
};
pub use planning::{PlanRequest, PlanResult};
pub use verification::VerificationRequest;

/// Explicit execution dependencies. `None` retains each operation's existing
/// phase deadline; a supplied deadline may only shorten those bounds.
#[derive(Clone)]
pub struct Config {
    pub device: DeviceConfig,
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
}

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

/// A concrete engine with caller-owned cancellation and an observation sink.
/// It never resolves CLI configuration, writes terminal output, or selects an
/// exit status. Observer callbacks must return normally; they convey no authority.
#[derive(Clone)]
pub struct Engine {
    config: Config,
    observer: Observer,
}

impl Engine {
    pub fn new(
        config: Config,
        observer: impl for<'a> Fn(Event<'a>) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            observer: Arc::new(observer),
        }
    }
    pub fn quiet(config: Config) -> Self {
        Self::new(config, |_| Ok(()))
    }
    pub fn plan(&self, request: PlanRequest<'_>) -> Result<PlanResult> {
        self.with_observer(|| planning::run(request, self))
    }
    pub fn plan_metadata(&self, request: MetadataPlanRequest<'_>) -> Result<MetadataPlanResult> {
        self.with_observer(|| planning::metadata_intent::run(request, self))
    }
    pub(crate) fn selected_device(&self) -> Option<&str> {
        self.config.device.selected_device()
    }
    /// The connection options a printed follow-up command needs
    /// (`DeviceConfig::command_options`).
    pub(crate) fn command_options(&self) -> String {
        self.config.device.command_options()
    }
    pub async fn verify(&self, request: VerificationRequest<'_>) -> Result<VerificationResult> {
        self.check()?;
        self.scope(Box::pin(verification::run(request, self))).await
    }
    // Keep concrete workflow state off the caller's stack before wrapping it in
    // task-local scopes. Ordinary frontend futures may compose several calls.
    /// Remove tracks and add the sources again as one replacement
    /// coordination (`sync --replace-library` or `--replace-changed`).
    pub async fn replace(
        &self,
        request: ReplaceRequest<'_>,
        confirm: &mut impl FnMut(ReplacePreview<'_>) -> Result<bool>,
    ) -> Result<ReplaceOutcome> {
        self.check()?;
        self.scope(Box::pin(crate::engine::execution::replace::run(
            request, self, confirm,
        )))
        .await
    }
    pub async fn sync(&self, request: SyncRequest<'_>) -> Result<SyncResult> {
        self.check()?;
        self.scope(Box::pin(crate::engine::execution::sync::run(request, self)))
            .await
    }
    /// Write the journal from a fresh capture, carrying an earlier journal
    /// forward (`rustunes pull`). Nothing is sent to the phone.
    pub async fn pull(&self, request: PullRequest<'_>) -> Result<PullResult> {
        self.check()?;
        self.scope(Box::pin(execution::pull::run(request, self)))
            .await
    }
    pub async fn cover(&self, request: CoverRequest<'_>) -> Result<CoverResult> {
        self.check()?;
        self.scope(Box::pin(crate::engine::execution::cover::run(
            request, self,
        )))
        .await
    }
    /// Scope an engine operation or CLI command to this explicit device
    /// configuration. Nested engines remain isolated from one another.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        let observer = Arc::clone(&self.observer);
        CURRENT_OBSERVER
            .scope(
                Arc::clone(&self.observer),
                crate::engine::execution::device::scope_with_observer(
                    self.config.device.clone(),
                    move |event| observer(Event::Device(event)),
                    future,
                ),
            )
            .await
    }
    /// Install this observer for synchronous work, including an explicitly
    /// cloned engine moved into a blocking task. The guard restores outer scope.
    pub(crate) fn with_observer<T>(&self, work: impl FnOnce() -> T) -> T {
        CURRENT_OBSERVER.sync_scope(Arc::clone(&self.observer), work)
    }
    pub(crate) fn emit(&self, event: Event<'_>) -> Result<()> {
        (self.observer)(event)
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            !self.config.cancelled.load(Ordering::Relaxed),
            "insertion cancelled; reconcile any Started attempt"
        );
        ensure!(
            self.config
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline),
            "original insertion deadline expired"
        );
        Ok(())
    }
    pub(crate) fn control(&self, seconds: u64) -> crate::engine::execution::native_insert::Control {
        let deadline = Instant::now() + std::time::Duration::from_secs(seconds);
        crate::engine::execution::native_insert::Control {
            deadline: self
                .config
                .deadline
                .map_or(deadline, |outer| deadline.min(outer)),
            cancelled: Arc::clone(&self.config.cancelled),
        }
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

impl Skipped {
    /// The plan's skipped sources.
    pub(crate) fn planned(plan: &crate::engine::planning::additive::Plan) -> Vec<Self> {
        plan.sources
            .iter()
            .filter_map(|row| match &row.decision {
                crate::engine::planning::additive::Decision::Skip {
                    code,
                    reason,
                    message,
                } => Some(Self {
                    path: row.relative_path.clone(),
                    code,
                    reason: reason.clone(),
                    detail: message.clone(),
                    stage: SkipStage::Plan,
                }),
                _ => None,
            })
            .collect()
    }
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

#[cfg(test)]
pub(crate) fn test_engine() -> Engine {
    Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    })
}
/// A test engine whose USB connections go to `socket`, where nothing
/// listens: a run that reaches for the phone fails there, whatever phone is
/// attached to the machine.
#[cfg(test)]
pub(crate) fn offline_test_engine(socket: &Path) -> Engine {
    Engine::quiet(Config {
        device: DeviceConfig::new(None, Some(socket.as_os_str().to_owned())).unwrap(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    })
}
#[cfg(test)]
mod tests;

pub(crate) mod execution;

pub(crate) mod recovery;
