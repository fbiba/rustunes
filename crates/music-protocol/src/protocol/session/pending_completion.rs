//! Pending-only completion. This wire mechanism confers no journal or body authority.
use super::super::trace::{OutputAllowance, OutputReservation};
use super::*;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

/// Observational inputs for the application's one-shot, journal-owned body callback.
/// No source, signer, writable transport, journal permit or serialization is exposed.
pub struct PendingBodyContext {
    own_asset_id: String,
    own_parts: u64,
    permanent_path: String,
    media_anchor: u32,
    ready_sha256: [u8; 32],
    manifest_sha256: [u8; 32],
    control: super::super::transfer::CaptureControl,
}
impl PendingBodyContext {
    pub fn own_asset_id(&self) -> &str {
        &self.own_asset_id
    }
    pub fn own_parts(&self) -> u64 {
        self.own_parts
    }
    pub fn permanent_path(&self) -> &str {
        &self.permanent_path
    }
    pub fn media_anchor(&self) -> u32 {
        self.media_anchor
    }
    pub fn ready_sha256(&self) -> [u8; 32] {
        self.ready_sha256
    }
    pub fn manifest_sha256(&self) -> [u8; 32] {
        self.manifest_sha256
    }
    pub fn control(&self) -> &super::super::transfer::CaptureControl {
        &self.control
    }
}

/// Bytes accepted by AsyncWrite are a local transport fact, not peer processing.
#[derive(Clone, Debug)]
pub struct PendingWriteObservation {
    pub frame_index: usize,
    pub command: String,
    pub asset_id: String,
    pub frame_bytes: usize,
    pub intended_sha256: [u8; 32],
    pub intended_trace_complete: bool,
    pub accepted_bytes: usize,
    pub accepted_sha256: Option<[u8; 32]>,
    pub flushed: bool,
}

#[derive(Clone, Debug)]
pub struct PendingReceivedObservation {
    pub frame_index: usize,
    pub frame_sha256: [u8; 32],
    /// State at the first byte actually consumed locally, not remote arrival time.
    pub started_after_full_settlement: bool,
}

#[derive(Clone, Debug)]
pub struct PendingPartialReceivedObservation {
    pub frame_index: usize,
    pub consumed_bytes: usize,
    pub consumed_sha256: [u8; 32],
    pub started_after_full_settlement: bool,
}

/// Factual evidence only. It cannot be loaded as a delivery or journal permit.
#[derive(Clone, Debug)]
pub struct PendingCompletionEvidence {
    pub own_asset_id: String,
    pub own_parts: u64,
    pub permanent_path: String,
    pub media_anchor: u32,
    pub ready_sha256: [u8; 32],
    pub manifest_frame_index: usize,
    pub manifest_sha256: [u8; 32],
    pub required_foreign_asset_ids: Vec<String>,
    pub body_verification_sha256: Option<[u8; 32]>,
    pub writes: Vec<PendingWriteObservation>,
    pub settled_foreign_asset_ids: Vec<String>,
    pub received: Vec<PendingReceivedObservation>,
    pub partial_receive: Option<PendingPartialReceivedObservation>,
    pub terminal: Option<PendingReceivedObservation>,
}

/// A post-body same-transport terminal observation after the entire refusal set.
/// It does not establish receiver readback, original-parent success or disposition.
pub struct PendingCompletionReceipt {
    evidence: PendingCompletionEvidence,
}
impl PendingCompletionReceipt {
    pub fn evidence(&self) -> &PendingCompletionEvidence {
        &self.evidence
    }
}

#[derive(Debug)]
pub struct PendingCompletionFailure {
    error: anyhow::Error,
    evidence: PendingCompletionEvidence,
}
impl PendingCompletionFailure {
    pub fn evidence(&self) -> &PendingCompletionEvidence {
        &self.evidence
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
}
impl std::fmt::Display for PendingCompletionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pending completion failed; transport closed: {:#}",
            self.error
        )
    }
}
impl std::error::Error for PendingCompletionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

type Evidence = Arc<Mutex<PendingCompletionEvidence>>;
fn snapshot(evidence: &Evidence) -> PendingCompletionEvidence {
    evidence
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}
fn update(evidence: &Evidence, f: impl FnOnce(&mut PendingCompletionEvidence)) {
    f(&mut evidence.lock().unwrap_or_else(|poison| poison.into_inner()));
}
fn remaining(control: &super::super::transfer::CaptureControl) -> Duration {
    control.deadline.saturating_duration_since(Instant::now())
}
fn reserve_frame(frames: &AtomicUsize) -> Result<()> {
    frames
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            (value < MAX_FRAMES).then_some(value + 1)
        })
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("pending ATC frame count exhausted"))
}

/// Force the selectable worker to suspend at every mutation boundary. On its
/// next poll the biased owner selector has polled receive first; if even a
/// partial incoming header was consumed, worker actions remain suspended until
/// the owner classifies that complete frame as allowed telemetry.
struct ReceiveCheckpoint {
    partial: AtomicBool,
    worker: Mutex<Option<std::task::Waker>>,
}
impl ReceiveCheckpoint {
    fn new() -> Self {
        Self {
            partial: AtomicBool::new(false),
            worker: Mutex::new(None),
        }
    }
    async fn observe_before_next_action(&self) {
        let mut suspended = false;
        std::future::poll_fn(|cx| {
            if !suspended {
                suspended = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let mut worker = self
                .worker
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if self.partial.load(Ordering::SeqCst) {
                *worker = Some(cx.waker().clone());
                Poll::Pending
            } else {
                *worker = None;
                Poll::Ready(())
            }
        })
        .await
    }
    fn started_incoming(&self) {
        self.partial.store(true, Ordering::SeqCst);
    }
    fn allowed_telemetry(&self) {
        self.partial.store(false, Ordering::SeqCst);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            worker.wake();
        }
    }
}

fn file_complete(asset_id: &str, permanent_path: &str) -> Value {
    let params = Dictionary::from_iter::<[(String, Value); 3]>([
        ("AssetID".into(), asset_id.into()),
        ("AssetPath".into(), permanent_path.into()),
        ("Dataclass".into(), "Media".into()),
    ]);
    Value::Dictionary(Dictionary::from_iter::<[(String, Value); 3]>([
        ("Session".into(), 2u64.into()),
        ("Command".into(), "FileComplete".into()),
        ("Params".into(), Value::Dictionary(params)),
    ]))
}
fn file_error(asset_id: &str) -> Value {
    let params = Dictionary::from_iter::<[(String, Value); 4]>([
        ("AssetID".into(), asset_id.into()),
        ("Dataclass".into(), "Media".into()),
        ("ErrorCode".into(), Value::Integer(16i64.into())),
        ("ErrorIsVisible".into(), false.into()),
    ]);
    Value::Dictionary(Dictionary::from_iter::<[(String, Value); 3]>([
        ("Session".into(), 2u64.into()),
        ("Command".into(), "FileError".into()),
        ("Params".into(), Value::Dictionary(params)),
    ]))
}

struct CurrentWrite {
    frame_index: usize,
    frame: Vec<u8>,
    accepted: Vec<u8>,
    retention_attempted: bool,
    accepted_reservation: Option<OutputReservation>,
}
struct PendingWriter {
    half: WriteHalf<Box<dyn ReadWrite>>,
    trace: PathBuf,
    frames: Arc<AtomicUsize>,
    indices: Arc<AtomicUsize>,
    evidence: Evidence,
    current: Option<CurrentWrite>,
    checkpoint: Arc<ReceiveCheckpoint>,
    output: Option<OutputAllowance>,
}
impl PendingWriter {
    fn retain_current(&mut self) -> Result<()> {
        if let Some(current) = self.current.as_mut() {
            if !current.retention_attempted && !current.accepted.is_empty() {
                current.retention_attempted = true;
                let suffix = if current.accepted.len() == current.frame.len() {
                    "accepted.atc"
                } else {
                    "accepted-partial.atc"
                };
                let path = self
                    .trace
                    .join(format!("{:03}-{suffix}", current.frame_index));
                if self.output.is_some() {
                    current
                        .accepted_reservation
                        .take()
                        .context("missing pending accepted-byte reservation")?
                        .write(&path, &current.accepted)?;
                } else {
                    private_write(&path, &current.accepted)?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for PendingWriter {
    fn drop(&mut self) {
        if self.output.is_some() {
            let _ = self.retain_current();
        }
    }
}

struct PendingReader {
    half: ReadHalf<Box<dyn ReadWrite>>,
    trace: PathBuf,
    frames: Arc<AtomicUsize>,
    indices: Arc<AtomicUsize>,
    reservation_held: bool,
    settled: Arc<AtomicBool>,
    evidence: Evidence,
    bytes: Vec<u8>,
    frame_index: Option<usize>,
    started_settled: bool,
    raw_attempted: bool,
    checkpoint: Arc<ReceiveCheckpoint>,
    output: Option<OutputAllowance>,
    raw_reservation: Option<OutputReservation>,
}
impl PendingReader {
    async fn read_until(&mut self, needed: usize) -> Result<()> {
        let mut scratch = [0u8; 8192];
        while self.bytes.len() < needed {
            let take = (needed - self.bytes.len()).min(scratch.len());
            let count = self.half.read(&mut scratch[..take]).await?;
            ensure!(
                count > 0,
                "pending ATC EOF with {} of {needed} bytes",
                self.bytes.len()
            );
            if self.frame_index.is_none() {
                self.frame_index = Some(self.indices.fetch_add(1, Ordering::SeqCst));
                self.started_settled = self.settled.load(Ordering::SeqCst);
                self.checkpoint.started_incoming();
            }
            self.bytes.extend_from_slice(&scratch[..count]);
            let partial = PendingPartialReceivedObservation {
                frame_index: self.frame_index.unwrap(),
                consumed_bytes: self.bytes.len(),
                consumed_sha256: Sha256::digest(&self.bytes).into(),
                started_after_full_settlement: self.started_settled,
            };
            update(&self.evidence, |value| {
                value.partial_receive = Some(partial)
            });
        }
        Ok(())
    }
    fn retain_partial(&mut self) -> Result<()> {
        if !self.bytes.is_empty() && !self.raw_attempted {
            self.raw_attempted = true;
            let path = self
                .trace
                .join(format!("{:03}-partial.atc", self.frame_index.unwrap()));
            if self.output.is_some() {
                self.raw_reservation
                    .take()
                    .context("missing pending partial reservation")?
                    .write(&path, &self.bytes)?;
            } else {
                private_write(&path, &self.bytes)?;
            }
        }
        Ok(())
    }
}

impl Drop for PendingReader {
    fn drop(&mut self) {
        if self.output.is_some() {
            let _ = self.retain_partial();
        }
    }
}
