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

impl PendingAudioDemand {
    /// Consume this exact live demand inside the application's authorized child
    /// continuation. The callback must capture its non-serializable journal permit
    /// and held source, verify body installation/readbacks and all required own-part
    /// handling, then return its factual verification receipt digest. Neither this
    /// context nor an arbitrary returned digest grants that authority.
    ///
    /// No deadline argument exists: demand's original absolute caller/channel
    /// deadline and cancellation Arc govern body, sends and terminal reception.
    /// Incoming requests/early terminals are monitored while the callback runs.
    /// Any failure consumes both halves; there is no ordinary Session fallback.
    pub async fn continue_body_and_complete<F, Fut>(
        self,
        permanent_path: &str,
        body: F,
    ) -> std::result::Result<PendingCompletionReceipt, PendingCompletionFailure>
    where
        F: FnOnce(PendingBodyContext) -> Fut,
        Fut: Future<Output = Result<[u8; 32]>>,
    {
        let evidence = Arc::new(Mutex::new(PendingCompletionEvidence {
            own_asset_id: self.own_asset_id.clone(),
            own_parts: self.manifest.own_parts(),
            permanent_path: permanent_path.to_owned(),
            media_anchor: self._session.prepared.media_anchor.parse().unwrap_or(0),
            ready_sha256: self.ready_receipt_sha256,
            manifest_frame_index: self.observation.frame_index,
            manifest_sha256: self.observation.frame_sha256,
            required_foreign_asset_ids: self.manifest.foreign_asset_ids().to_vec(),
            body_verification_sha256: None,
            writes: Vec::new(),
            settled_foreign_asset_ids: Vec::new(),
            received: Vec::new(),
            partial_receive: None,
            terminal: None,
        }));
        let result = async {
            ensure!(self._session.phase == Phase::PendingDemandObserved
                && self._session.prepared.metadata_mode == MetadataMode::EmptyDelta,
                "pending completion requires its actual empty-delta manifest phase");
            validate_music_path(permanent_path)?;
            let media_anchor = self._session.prepared.media_anchor.parse::<u32>()?;
            self.original_control.check()?;
            let control = self.original_control;
            ensure!(control.deadline <= self._session.channel.deadline,
                "pending completion cannot extend its original channel deadline");
            let context = PendingBodyContext {
                own_asset_id: self.own_asset_id.clone(), own_parts: self.manifest.own_parts(),
                permanent_path: permanent_path.to_owned(), media_anchor,
                ready_sha256: self.ready_receipt_sha256,
                manifest_sha256: self.observation.frame_sha256, control: control.clone(),
            };
            let own_asset_id = self.own_asset_id;
            let foreign = self.manifest.foreign_asset_ids().to_vec();
            let channel = self._session.channel;
            let (read, write, output) = channel.client.into_pending_parts(&control)?;
            let frames = Arc::new(AtomicUsize::new(channel.frames));
            let indices = Arc::new(AtomicUsize::new(channel.frames));
            let settled = Arc::new(AtomicBool::new(false));
            let checkpoint = Arc::new(ReceiveCheckpoint::new());
            let mut reader = PendingReader {
                half: read, trace: channel.trace.clone(), frames: Arc::clone(&frames),
                indices: Arc::clone(&indices), reservation_held: false,
                settled: Arc::clone(&settled), evidence: Arc::clone(&evidence),
                checkpoint: Arc::clone(&checkpoint),
                bytes: Vec::new(), frame_index: None, started_settled: false,
                raw_attempted: false, output: output.clone(), raw_reservation: None,
            };
            let mut writer = PendingWriter {
                half: write, trace: channel.trace, frames, indices,
                evidence: Arc::clone(&evidence), current: None,
                checkpoint: Arc::clone(&checkpoint), output,
            };
            let wire_result = async {
                let work = async {
                    control.run(remaining(&control), "pending initial receive boundary",
                        async { checkpoint.observe_before_next_action().await; Ok(()) }).await?;
                    let digest = control.run(remaining(&control), "pending body deadline",
                        body(context)).await?;
                    update(&evidence, |value| value.body_verification_sha256 = Some(digest));
                    control.run(remaining(&control), "pending callback receive boundary",
                        async { checkpoint.observe_before_next_action().await; Ok(()) }).await?;
                    writer.send(&file_complete(&own_asset_id, permanent_path), &control).await?;
                    for asset_id in &foreign {
                        writer.send(&file_error(asset_id), &control).await?;
                        update(&evidence, |value| value.settled_foreign_asset_ids.push(asset_id.clone()));
                    }
                    control.check()?;
                    ensure!(snapshot(&evidence).settled_foreign_asset_ids == foreign,
                        "pending foreign set is incomplete or reordered");
                    settled.store(true, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                };
                tokio::pin!(work);
                let mut work_done = false;
                let mut telemetry = 0usize;
                loop {
                    tokio::select! {
                        // Every callback/write/chunk/flush boundary forces work
                        // Pending, so this priority applies even to ready chains.
                        biased;
                        received = reader.receive(&control) => {
                            let (message, observed) = received?;
                            let root = dictionary(&message)?;
                            ensure!(number(root, "Session")? == 2,
                                "wrong-session pending completion message");
                            match string(root, "Command")? {
                                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                                    check_telemetry(root)?;
                                    telemetry += 1;
                                    ensure!(telemetry <= 64, "pending completion telemetry bound");
                                    // Release outgoing work only after the owner
                                    // has classified the complete incoming frame.
                                    checkpoint.allowed_telemetry();
                                }
                                "SyncFinished" => {
                                    ensure!(work_done && settled.load(Ordering::SeqCst)
                                        && observed.started_after_full_settlement,
                                        "early pending SyncFinished before body and full settlement");
                                    if root.contains_key("Params") { child(root, "Params")?; }
                                    let recorded = snapshot(&evidence);
                                    ensure!(recorded.body_verification_sha256.is_some()
                                        && recorded.settled_foreign_asset_ids == foreign
                                        && recorded.writes.len() == 1 + foreign.len()
                                        && recorded.writes.iter().all(|frame|
                                            frame.accepted_bytes == frame.frame_bytes && frame.flushed),
                                        "pending terminal cannot qualify an unsettled outgoing set");
                                    control.check()?;
                                    update(&evidence, |value| value.terminal = Some(observed));
                                    break Ok(());
                                }
                                _ => bail!("unexpected pending terminal/request; no further sends"),
                            }
                        }
                        completed = &mut work, if !work_done => {
                            completed?;
                            work_done = true;
                        }
                    }
                }
            }.await; // Work is dropped before inspecting its writer's accepted prefix.
            let read_retention = reader.retain_partial();
            let write_retention = writer.retain_current();
            // Both split halves drop at this scope's end, before any receipt/error
            // escapes. Full/partial evidence is never a retry or reuse capability.
            match (wire_result, read_retention, write_retention) {
                (Ok(()), Ok(()), Ok(())) => Ok(()),
                (wire, read, write) => {
                    let mut errors = Vec::new();
                    if let Err(error) = wire { errors.push(format!("{error:#}")); }
                    if let Err(error) = read { errors.push(format!("read retention: {error:#}")); }
                    if let Err(error) = write { errors.push(format!("write retention: {error:#}")); }
                    bail!("{}", errors.join("; "))
                }
            }
        }.await;
        let evidence = snapshot(&evidence);
        match result {
            Ok(()) => Ok(PendingCompletionReceipt { evidence }),
            Err(error) => Err(PendingCompletionFailure { error, evidence }),
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
    async fn send(
        &mut self,
        message: &Value,
        control: &super::super::transfer::CaptureControl,
    ) -> Result<()> {
        control.check()?;
        ensure!(
            self.current.is_none(),
            "pending write cannot be reused after failure"
        );
        let bounded_index = if self.output.is_some() {
            reserve_frame(&self.frames)?;
            Some(self.indices.fetch_add(1, Ordering::SeqCst))
        } else {
            None
        };
        let frame = if let Some(output) = &self.output {
            super::super::framing::retain_outgoing_raw(
                message,
                &self.trace,
                bounded_index.unwrap(),
                output,
                control,
            )?
        } else {
            let mut payload = Vec::new();
            message.to_writer_binary(&mut payload)?;
            ensure!(
                !payload.is_empty() && payload.len() <= MAX_BYTES,
                "pending outgoing frame bound"
            );
            let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
            frame.extend_from_slice(&payload);
            frame
        };
        let frame_index = if let Some(index) = bounded_index {
            index
        } else {
            reserve_frame(&self.frames)?;
            self.indices.fetch_add(1, Ordering::SeqCst)
        };
        let root = dictionary(message)?;
        let observation = PendingWriteObservation {
            frame_index,
            command: string(root, "Command")?.to_owned(),
            asset_id: string(child(root, "Params")?, "AssetID")?.to_owned(),
            frame_bytes: frame.len(),
            intended_sha256: Sha256::digest(&frame).into(),
            intended_trace_complete: false,
            accepted_bytes: 0,
            accepted_sha256: None,
            flushed: false,
        };
        let observation_index = snapshot(&self.evidence).writes.len();
        update(&self.evidence, |value| value.writes.push(observation));
        self.current = Some(CurrentWrite {
            frame_index,
            frame,
            accepted: Vec::new(),
            retention_attempted: false,
            accepted_reservation: None,
        });
        let result = control
            .run(remaining(control), "pending send deadline", async {
                let current = self.current.as_mut().unwrap();
                if let Some(output) = &self.output {
                    // Protect every possible accepted byte before the first write.
                    // The reader cannot spend this capacity while a send is pending.
                    current.accepted_reservation = Some(
                        output
                            .reserve_file(current.frame.len() as u64, current.frame.len() as u64)?,
                    );
                    let mut xml = output.create_controlled(
                        &self.trace.join(format!("{frame_index:03}-outgoing.plist")),
                        control,
                    )?;
                    message.to_writer_xml(&mut xml)?;
                    xml.finish()?;
                } else {
                    private_write(
                        &self.trace.join(format!("{frame_index:03}-outgoing.atc")),
                        &current.frame,
                    )?;
                    let mut xml = Vec::new();
                    message.to_writer_xml(&mut xml)?;
                    private_write(
                        &self.trace.join(format!("{frame_index:03}-outgoing.plist")),
                        &xml,
                    )?;
                }
                update(&self.evidence, |value| {
                    value.writes[observation_index].intended_trace_complete = true
                });
                self.checkpoint.observe_before_next_action().await;
                while current.accepted.len() < current.frame.len() {
                    let before = current.accepted.len();
                    let count = self.half.write(&current.frame[before..]).await?;
                    ensure!(count > 0, "pending ATC write accepted zero bytes");
                    current
                        .accepted
                        .extend_from_slice(&current.frame[before..before + count]);
                    update(&self.evidence, |value| {
                        let observed = &mut value.writes[observation_index];
                        observed.accepted_bytes = current.accepted.len();
                        observed.accepted_sha256 = Some(Sha256::digest(&current.accepted).into());
                    });
                    // A ready write hook can make an early incoming command readable.
                    // Always re-enter receive priority before more bytes or flush.
                    self.checkpoint.observe_before_next_action().await;
                }
                self.half.flush().await?;
                update(&self.evidence, |value| {
                    value.writes[observation_index].flushed = true
                });
                self.checkpoint.observe_before_next_action().await;
                Ok(())
            })
            .await;
        let retained = self.retain_current();
        match (result, retained) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Err(error), Err(retention)) => {
                bail!("{error:#}; accepted-byte retention: {retention:#}")
            }
        }
        self.current = None;
        Ok(())
    }
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
    async fn receive(
        &mut self,
        control: &super::super::transfer::CaptureControl,
    ) -> Result<(Value, PendingReceivedObservation)> {
        // A work-completion branch may drop this borrowed receive future. Every
        // successful read has synchronously updated owned bytes; no read_exact
        // future owns an unrecorded prefix. Resume only inside this consumed path.
        control
            .run(remaining(control), "pending terminal deadline", async {
                if !self.reservation_held {
                    // Reserve a bounded slot before starting a read; assign its
                    // chronological trace index only after actual bytes are consumed.
                    reserve_frame(&self.frames)?;
                    if let Some(output) = &self.output {
                        // Only the header is held while waiting. Reserving an entire
                        // frame here could starve the concurrent completion writer.
                        self.raw_reservation = Some(output.reserve_file(4, 4)?);
                    }
                    self.reservation_held = true;
                }
                self.read_until(4).await?;
                let length = u32::from_le_bytes(self.bytes[..4].try_into()?) as usize;
                ensure!(
                    length > 0 && length <= MAX_BYTES,
                    "pending incoming frame bound"
                );
                if self.output.is_some() {
                    self.raw_reservation
                        .as_mut()
                        .context("missing pending header reservation")?
                        .grow_to((4 + length) as u64)?;
                }
                self.read_until(4 + length).await?;
                let frame_index = self.frame_index.unwrap();
                self.raw_attempted = true;
                let raw_path = self.trace.join(format!("{frame_index:03}-received.atc"));
                if self.output.is_some() {
                    self.raw_reservation
                        .take()
                        .context("missing pending raw reservation")?
                        .write(&raw_path, &self.bytes)?;
                } else {
                    private_write(&raw_path, &self.bytes)?;
                }
                let observed = PendingReceivedObservation {
                    frame_index,
                    frame_sha256: Sha256::digest(&self.bytes).into(),
                    started_after_full_settlement: self.started_settled,
                };
                update(&self.evidence, |value| {
                    value.received.push(observed.clone());
                    value.partial_receive = None;
                });
                let message: Value = plist::from_bytes(&self.bytes[4..])
                    .context("invalid pending incoming plist")?;
                let xml_path = self.trace.join(format!("{frame_index:03}-received.plist"));
                if let Some(output) = &self.output {
                    let mut xml = output.create_controlled(&xml_path, control)?;
                    message.to_writer_xml(&mut xml)?;
                    xml.finish()?;
                } else {
                    let mut xml = Vec::new();
                    message.to_writer_xml(&mut xml)?;
                    private_write(&xml_path, &xml)?;
                }
                self.bytes.clear();
                self.frame_index = None;
                self.reservation_held = false;
                self.raw_attempted = false;
                let root = dictionary(&message)?;
                let command = string(root, "Command")?;
                let without_params = command == "Pong"
                    || (command == "SyncFinished" && !root.contains_key("Params"));
                exact_keys(
                    root,
                    if without_params {
                        &["Command", "Session", "Type", "Id"]
                    } else {
                        &["Command", "Session", "Type", "Id", "Params"]
                    },
                )?;
                ensure!(
                    number(root, "Type")? == 0,
                    "unsupported pending incoming message type"
                );
                number(root, "Id")?;
                number(root, "Session")?;
                Ok((message, observed))
            })
            .await
    }
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
