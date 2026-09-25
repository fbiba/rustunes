//! Bounded explicit-handle AFC operations.
//! Absence checks plus non-truncating Rw opens rely on exclusive writer ownership;
//! AFC's verified interface has no atomic create-exclusive or no-clobber rename.
//! Source bounds do not bound upstream idevice's malformed-frame allocation.

use super::afc;
use super::afc::copy::{CHUNK, OPERATION};

use super::trace::{OutputAllowance, OutputReservation, private_write};
use anyhow::{Context, Result, bail, ensure};
use idevice::{
    IdeviceError,
    afc::{
        AfcClient,
        errors::AfcError,
        opcode::{AfcFopenMode, AfcOpcode},
        packet::AfcPacket,
    },
    provider::IdeviceProvider,
};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

pub const MAX_ASSET: u64 = 32 * 1024 * 1024;
/// Separate production audio bound; every other write_new (covers, the
/// metadata pair) keeps MAX_ASSET, 32 MiB.
pub const MAX_AUDIO: u64 = 64 * 1024 * 1024;
pub const MAX_CATALOG: u64 = 64 * 1024 * 1024;
const MAX_OPS: usize = 4096;
const SUMMARY_FRAMES: usize = 8;
const SUMMARY_HEADER: usize = 256;
pub const MAX_BOUNDED_OPS: usize = 8192;
/// One batch session's guard connection, which checks every member's
/// namespace and cover: up to a full operation file's members.
pub const MAX_SESSION_OPS: usize = 32768;
pub const METADATA: &str = "iTunes_Control/Sync/Media/Sync_00000000.plist";
pub const SIGNATURE: &str = "iTunes_Control/Sync/Media/Sync_00000000.plist.cig";

pub type Attributes = BTreeMap<String, String>;
enum Reply {
    Packet(AfcPacket),
    NotFound,
}

/// Observation from the initial GetFileInfo of a bounded read only. The sole
/// constructor is stat(None) inside read_inner; transport, later-open/read and
/// changed/disappeared-after-read errors never create this type. No authority,
/// serialization, Clone or public constructor is provided.
#[derive(Debug)]
pub struct ReadbackPathAbsent {
    path: String,
    expected_bytes: u64,
    operation_sequence: usize,
}
impl ReadbackPathAbsent {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn expected_bytes(&self) -> u64 {
        self.expected_bytes
    }
    pub fn operation_sequence(&self) -> usize {
        self.operation_sequence
    }
}
impl std::fmt::Display for ReadbackPathAbsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the existing diagnostic while exposing the typed cause.
        f.write_str("readback path absent")
    }
}
impl std::error::Error for ReadbackPathAbsent {}

/// Maximum bytes in one retained Current-profile metadata observation.
pub const CURRENT_METADATA_LIMIT: usize = 16 * 1024;
pub(crate) const CURRENT_READ_RECEIPTS: usize = 8;

/// Cooperative control only. This type confers no captured-state authority.
#[derive(Clone)]
pub struct CaptureControl {
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}
impl CaptureControl {
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            "Current capture cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "Current capture deadline expired"
        );
        Ok(())
    }
    pub async fn run<T, F>(&self, cap: Duration, name: &'static str, operation: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        self.check()?;
        let cancel = async {
            while !self.cancelled.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let result = tokio::select! {
            result = timeout_at(self.deadline.min(Instant::now() + cap), operation) => {
                result.context(name)?
            }
            _ = cancel => bail!("Current capture cancelled; pending connection discarded"),
        };
        self.check()?;
        result
    }
}

/// A bounded writer for Current-profile metadata observations.
#[derive(Default)]
pub struct CappedMetadata(Vec<u8>);
impl CappedMetadata {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}
impl std::io::Write for CappedMetadata {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > CURRENT_METADATA_LIMIT.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("Current metadata byte cap exceeded"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct LimitedText(String);
impl std::fmt::Write for LimitedText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let remaining = 2048usize.saturating_sub(self.0.len());
        let mut take = remaining.min(text.len());
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        self.0.push_str(&text[..take]);
        if take < text.len() {
            Err(std::fmt::Error)
        } else {
            Ok(())
        }
    }
}
/// Render an error chain within the existing UTF-8-safe diagnostic bound.
pub fn bounded_error(error: &anyhow::Error) -> String {
    use std::fmt::Write as _;
    let mut output = LimitedText(String::new());
    let _ = write!(&mut output, "{error:#}");
    output.0
}
fn bounded_debug(error: &impl std::fmt::Debug) -> String {
    use std::fmt::Write as _;
    let mut output = LimitedText(String::new());
    let _ = write!(&mut output, "{error:?}");
    output.0
}
/// Encode one observation using the Current-profile output bound.
pub fn current_json_bytes(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    let mut output = CappedMetadata::new();
    serde_json::to_writer(&mut output, value)?;
    Ok(output.0)
}
/// Persist one bounded observation at an explicit, previously unused path.
pub fn write_current_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    private_write(path, &current_json_bytes(value)?)
}

/// Evidence profile for the opt-in shared-output transport. All profiles debit
/// the same caller-owned allowance; Current retains its additional local caps.
#[derive(Clone, Copy)]
pub enum EvidenceMode {
    Detailed,
    ReadonlySummary,
    CurrentSummary,
}

pub struct Transfer {
    afc: AfcClient,
    deadline: Instant,
    trace: PathBuf,
    sequence: usize,
    usable: bool,
    operation_limit: usize,
    cancelled: Option<Arc<AtomicBool>>,
    readonly_summary: bool,
    current_summary: bool,
    recent_frames: VecDeque<Value>,
    readbacks: usize,
    failure_recorded: bool,
    active_read: Option<Dictionary>,
    output: Option<OutputAllowance>,
    summary_failure: Option<OutputReservation>,
    /// Every connection starts locked: it can read, stat and list, and remove
    /// the one path `checked_removal` names, but not create, write, rename or
    /// make a directory. Only `Session` unlocks it, for a Started permit whose
    /// signature that session made.
    writable: bool,
    /// The one path a locked connection may remove next: set only by
    /// `remove_staged_operation`, after it read the file and found the
    /// caller's exact bytes, and taken by the very next request.
    checked_removal: Option<String>,
}

impl Transfer {
    /// Connect only an in-memory AFC endpoint for offline application tests.
    /// The default-off feature exposes no device connector or session authority.
    /// The caller owns the private trace and supplies one unchanged control.
    #[cfg(any(test, feature = "offline-test-support"))]
    pub fn from_test_duplex(
        endpoint: tokio::io::DuplexStream,
        trace: &Path,
        control: &CaptureControl,
        operations: usize,
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_SESSION_OPS).contains(&operations),
            "AFC operation allowance exceeds bound"
        );
        control.check()?;
        Ok(Self {
            afc: AfcClient::new(idevice::Idevice::new(Box::new(endpoint), "offline-only")),
            deadline: control.deadline,
            trace: trace.to_owned(),
            sequence: 0,
            usable: true,
            operation_limit: operations,
            cancelled: Some(control.cancelled.clone()),
            readonly_summary: false,
            current_summary: false,
            recent_frames: VecDeque::new(),
            readbacks: 0,
            failure_recorded: false,
            active_read: None,
            output: None,
            summary_failure: None,
            writable: false,
            checked_removal: None,
        })
    }

    /// Connect with one shared aggregate evidence allowance and the caller's
    /// original deadline/cancellation. No operation replenishes either budget.
    pub async fn connect_with_output(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        control: &CaptureControl,
        operations: usize,
        mode: EvidenceMode,
        output: OutputAllowance,
    ) -> Result<Self> {
        let mut transfer = control
            .run(
                Duration::from_secs(20),
                "AFC bounded-output connect",
                Self::connect_options(
                    provider,
                    trace,
                    control.deadline,
                    operations,
                    Some(control.cancelled.clone()),
                    !matches!(mode, EvidenceMode::Detailed),
                ),
            )
            .await?;
        transfer.current_summary = matches!(mode, EvidenceMode::CurrentSummary);
        transfer.configure_output(output)?;
        Ok(transfer)
    }

    #[cfg(any(test, feature = "offline-test-support"))]
    pub fn from_test_duplex_with_output(
        endpoint: tokio::io::DuplexStream,
        trace: &Path,
        control: &CaptureControl,
        operations: usize,
        mode: EvidenceMode,
        output: OutputAllowance,
    ) -> Result<Self> {
        let mut transfer = Self::from_test_duplex(endpoint, trace, control, operations)?;
        transfer.readonly_summary = !matches!(mode, EvidenceMode::Detailed);
        transfer.current_summary = matches!(mode, EvidenceMode::CurrentSummary);
        transfer.configure_output(output)?;
        Ok(transfer)
    }

    /// Lift the write lock. Only `Session::stage_metadata` and
    /// `Session::permit_writes` call this, after checking a Started permit's
    /// signature; the lock has no other way off.
    pub(crate) fn unlock_writes(&mut self) {
        self.writable = true;
    }

    fn configure_output(&mut self, output: OutputAllowance) -> Result<()> {
        if self.readonly_summary {
            // Held before any request. Other writers cannot consume the failure
            // record's capacity while this connection is collecting a summary.
            self.summary_failure = Some(output.reserve_file(1, CURRENT_METADATA_LIMIT as u64)?);
        }
        self.output = Some(output);
        Ok(())
    }

    pub async fn connect(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        deadline: Instant,
    ) -> Result<Self> {
        Self::connect_options(provider, trace, deadline, MAX_OPS, None, false).await
    }

    /// One fixed, precomputed allowance. No operation extends this budget or
    /// its original deadline. Cancellation discards rather than cleans up.
    pub async fn connect_bounded(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        deadline: Instant,
        operations: usize,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        Self::connect_options(
            provider,
            trace,
            deadline,
            operations,
            Some(cancelled),
            false,
        )
        .await
    }

    /// Read-only, bounded verification without per-chunk files or payload logs.
    /// Each completed read retains one hash/attributes receipt. At most eight
    /// recent frame headers are retained in memory and one bounded diagnostic is
    /// saved on failure. A failure discards this connection without cleanup I/O.
    /// Use a fresh trace directory for each connection/batch.
    pub async fn connect_bounded_readonly_summary(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        deadline: Instant,
        operations: usize,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        Self::connect_options(provider, trace, deadline, operations, Some(cancelled), true).await
    }

    /// The Current producer's distinct finite output profile. Existing summary
    /// and detailed APIs keep their historical evidence behavior.
    pub async fn connect_current_summary(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        control: &CaptureControl,
    ) -> Result<Self> {
        let mut transfer = control
            .run(
                Duration::from_secs(20),
                "Current AFC connect deadline",
                Self::connect_bounded_readonly_summary(
                    provider,
                    trace,
                    control.deadline,
                    MAX_BOUNDED_OPS,
                    control.cancelled.clone(),
                ),
            )
            .await?;
        transfer.current_summary = true;
        Ok(transfer)
    }

    async fn connect_options(
        provider: &dyn IdeviceProvider,
        trace: &Path,
        deadline: Instant,
        operations: usize,
        cancelled: Option<Arc<AtomicBool>>,
        readonly_summary: bool,
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_SESSION_OPS).contains(&operations),
            "AFC operation allowance exceeds bound"
        );
        ensure!(
            !cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed)),
            "AFC connection cancelled"
        );
        ensure!(
            Instant::now() < deadline,
            "AFC deadline expired before connect"
        );
        let afc = timeout_at(
            deadline.min(Instant::now() + Duration::from_secs(20)),
            crate::protocol::afc::connect_checked(provider),
        )
        .await
        .context("AFC connection deadline")??;
        Ok(Self {
            afc,
            deadline,
            trace: trace.to_owned(),
            sequence: 0,
            usable: true,
            operation_limit: operations,
            cancelled,
            readonly_summary,
            current_summary: false,
            recent_frames: VecDeque::new(),
            readbacks: 0,
            failure_recorded: false,
            active_read: None,
            output: None,
            summary_failure: None,
            writable: false,
            checked_removal: None,
        })
    }

    fn summary_result<T>(&mut self, result: Result<T>) -> Result<T> {
        if !self.readonly_summary || result.is_ok() || self.failure_recorded {
            return result;
        }
        self.usable = false;
        self.failure_recorded = true;
        let mut record = Dictionary::new();
        record.insert("OperationsAttempted".into(), (self.sequence as u64).into());
        record.insert(
            "OperationLimit".into(),
            (self.operation_limit as u64).into(),
        );
        record.insert("ConnectionDiscarded".into(), true.into());
        if let Some(read) = &self.active_read {
            record.insert("Readback".into(), Value::Dictionary(read.clone()));
        }
        // Bound both the file and the saved error, including upstream errors
        // which might contain a representation of untrusted protocol bytes.
        let error = if self.current_summary || self.output.is_some() {
            bounded_error(result.as_ref().err().unwrap())
        } else {
            format!("{:#}", result.as_ref().err().unwrap())
        };
        record.insert(
            "Error".into(),
            error.chars().take(2048).collect::<String>().into(),
        );
        record.insert(
            "RecentFrames".into(),
            Value::Array(self.recent_frames.iter().cloned().collect()),
        );
        let saved = if let Some(reservation) = self.summary_failure.take() {
            (|| {
                let mut file = reservation.create(&self.trace.join("afc-summary-failure.plist"))?;
                Value::Dictionary(record).to_writer_xml(&mut file)?;
                file.finish()
            })()
        } else {
            self.summary_write("afc-summary-failure.plist", record)
        };
        if let Err(log_error) = saved {
            return result.context(format!("summary diagnostic also failed: {log_error:#}"));
        }
        result
    }

    fn summary_write(&self, name: &str, record: Dictionary) -> Result<()> {
        if self.current_summary {
            ensure!(
                name == "afc-summary-failure.plist"
                    || (self.readbacks < CURRENT_READ_RECEIPTS
                        && name == format!("afc-readback-{:04}.plist", self.readbacks)),
                "Current summary receipt allowance exhausted"
            );
        }
        if let Some(output) = &self.output {
            // Failure diagnostics may follow cancellation or expiry. They still
            // debit the same aggregate, but cannot require live capture control.
            let path = self.trace.join(name);
            let mut file = if name == "afc-summary-failure.plist" {
                output.create(&path)?
            } else {
                output.create_controlled(&path, &self.evidence_control())?
            };
            if self.current_summary {
                file = file.limit_bytes(CURRENT_METADATA_LIMIT as u64);
            }
            Value::Dictionary(record).to_writer_xml(&mut file)?;
            file.finish()
        } else if self.current_summary {
            let mut xml = CappedMetadata::new();
            Value::Dictionary(record).to_writer_xml(&mut xml)?;
            private_write(&self.trace.join(name), xml.bytes())
        } else {
            let mut xml = Vec::new();
            Value::Dictionary(record).to_writer_xml(&mut xml)?;
            private_write(&self.trace.join(name), &xml)
        }
    }

    fn evidence_control(&self) -> CaptureControl {
        CaptureControl {
            deadline: self.deadline,
            cancelled: self
                .cancelled
                .as_ref()
                .expect("bounded-output cancellation")
                .clone(),
        }
    }
    fn evidence_xml(&self, name: &str, value: &Value) -> Result<()> {
        let path = self.trace.join(name);
        if let Some(output) = &self.output {
            let mut file = output.create_controlled(&path, &self.evidence_control())?;
            value.to_writer_xml(&mut file)?;
            file.finish()
        } else {
            let mut xml = Vec::new();
            value.to_writer_xml(&mut xml)?;
            private_write(&path, &xml)
        }
    }
    fn evidence_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        match &self.output {
            Some(output) => output.write(&self.trace.join(name), bytes),
            None => private_write(&self.trace.join(name), bytes),
        }
    }
    fn evidence_reply(
        &self,
        sequence: usize,
        reply: &AfcPacket,
        reservation: Option<OutputReservation>,
    ) -> Result<()> {
        let path = self.trace.join(format!("afc-{sequence:04}-reply.bin"));
        if let Some(reservation) = reservation {
            use std::io::Write;
            let mut file = reservation.create(&path)?;
            // Same bytes as AfcPacket::serialize(), without duplicating an
            // untrusted full reply before the output bound can refuse it.
            for field in [
                reply.header.magic,
                reply.header.entire_len,
                reply.header.header_payload_len,
                reply.header.packet_num,
                reply.header.operation as u64,
            ] {
                file.write_all(&field.to_le_bytes())?;
            }
            file.write_all(&reply.header_payload)?;
            file.write_all(&reply.payload)?;
            file.finish()
        } else {
            private_write(&path, &reply.serialize())
        }
    }

    fn evidence_reserved_xml(
        &self,
        name: &str,
        value: &Value,
        reservation: Option<OutputReservation>,
    ) -> Result<()> {
        if let Some(reservation) = reservation {
            let mut file = reservation.create(&self.trace.join(name))?;
            value.to_writer_xml(&mut file)?;
            file.finish()
        } else {
            self.evidence_xml(name, value)
        }
    }
    fn evidence_reserved_bytes(
        &self,
        name: &str,
        bytes: &[u8],
        reservation: Option<OutputReservation>,
    ) -> Result<()> {
        if let Some(reservation) = reservation {
            let mut end = bytes.len().min(reservation.capacity() as usize);
            if let Ok(text) = std::str::from_utf8(bytes) {
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
            }
            reservation.write(&self.trace.join(name), &bytes[..end])?;
            ensure!(
                end == bytes.len(),
                "AFC diagnostic truncated at reserved output allowance"
            );
            Ok(())
        } else {
            self.evidence_bytes(name, bytes)
        }
    }

    fn summary_frame(&mut self, sequence: usize, operation: AfcOpcode, header: &[u8]) {
        let mut frame = Dictionary::new();
        frame.insert("Sequence".into(), (sequence as u64).into());
        frame.insert("RequestOpcode".into(), (operation as u64).into());
        frame.insert("RequestHeaderBytes".into(), (header.len() as u64).into());
        frame.insert(
            "RequestHeaderPrefix".into(),
            Value::Data(header[..header.len().min(SUMMARY_HEADER)].to_vec()),
        );
        if self.recent_frames.len() == SUMMARY_FRAMES {
            self.recent_frames.pop_front();
        }
        self.recent_frames.push_back(Value::Dictionary(frame));
    }

    fn summary_reply(&mut self, reply: &AfcPacket) {
        let frame = self
            .recent_frames
            .back_mut()
            .unwrap()
            .as_dictionary_mut()
            .unwrap();
        frame.insert("ReplyOpcode".into(), (reply.header.operation as u64).into());
        frame.insert("PacketNumber".into(), reply.header.packet_num.into());
        frame.insert("EntireLength".into(), reply.header.entire_len.into());
        frame.insert(
            "HeaderLength".into(),
            reply.header.header_payload_len.into(),
        );
        frame.insert(
            "ReplyHeaderBytes".into(),
            (reply.header_payload.len() as u64).into(),
        );
        frame.insert(
            "ReplyHeaderPrefix".into(),
            Value::Data(
                reply.header_payload[..reply.header_payload.len().min(SUMMARY_HEADER)].to_vec(),
            ),
        );
        frame.insert(
            "ReplyPayloadBytes".into(),
            (reply.payload.len() as u64).into(),
        );
    }

    async fn op(
        &mut self,
        operation: AfcOpcode,
        header: Vec<u8>,
        payload: Vec<u8>,
    ) -> Result<Reply> {
        self.op_evidence(operation, header, payload, true).await
    }

    async fn op_evidence(
        &mut self,
        operation: AfcOpcode,
        header: Vec<u8>,
        payload: Vec<u8>,
        raw_reply: bool,
    ) -> Result<Reply> {
        let result = self
            .op_exchange(operation, header, payload, raw_reply)
            .await;
        self.summary_result(result)
    }

    async fn op_exchange(
        &mut self,
        operation: AfcOpcode,
        header: Vec<u8>,
        payload: Vec<u8>,
        raw_reply: bool,
    ) -> Result<Reply> {
        // Below every public method: no mutating request leaves a locked
        // connection, whichever path built it, but the one checked removal.
        // Nothing is counted or sent.
        let checked = self.checked_removal.take();
        ensure!(
            self.writable
                || !mutates(operation, &header)
                || (operation == AfcOpcode::RemovePath
                    && checked.is_some_and(|path| path.as_bytes() == header.as_slice())),
            "AFC {operation:?} refused: no Started permit unlocked this connection"
        );
        ensure!(self.usable, "AFC session already discarded");
        self.usable = false;
        if self.readonly_summary {
            readonly_operation(operation, &header, &payload)?;
        }
        ensure!(
            !self
                .cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed)),
            "AFC operation cancelled; connection discarded"
        );
        ensure!(
            self.sequence < self.operation_limit && Instant::now() < self.deadline,
            "AFC operation/deadline budget exhausted"
        );
        let sequence = self.sequence;
        self.sequence += 1;
        if self.readonly_summary {
            self.summary_frame(sequence, operation, &header);
        } else {
            let mut intent = Dictionary::new();
            intent.insert("Opcode".into(), (operation as u64).into());
            intent.insert("Header".into(), Value::Data(header.clone()));
            intent.insert("PayloadBytes".into(), (payload.len() as u64).into());
            intent.insert(
                "PayloadSHA256".into(),
                hex(&Sha256::digest(&payload)).into(),
            );
            self.evidence_xml(
                &format!("afc-{sequence:04}-intent.plist"),
                &Value::Dictionary(intent),
            )?;
        }
        let reply_reservation = if !self.readonly_summary {
            self.output
                .as_ref()
                .map(|output| output.reserve_file(40, u64::MAX))
                .transpose()?
        } else {
            None
        };
        let result = if self.current_summary || self.output.is_some() {
            let control = CaptureControl {
                deadline: self.deadline,
                cancelled: self
                    .cancelled
                    .as_ref()
                    .expect("Current cancellation")
                    .clone(),
            };
            // Preserve status classification; control failures make the session unusable.
            let exchange = control
                .run(OPERATION, "Current AFC operation deadline", async {
                    Ok(self.afc.send_op(operation, header, payload).await)
                })
                .await;
            match exchange {
                Ok(reply) => Ok(reply),
                Err(error) => {
                    if self.output.is_some() && !self.readonly_summary {
                        self.evidence_reserved_bytes(
                            &format!("afc-{sequence:04}-error.txt"),
                            bounded_error(&error).as_bytes(),
                            reply_reservation,
                        )?;
                    }
                    return Err(error);
                }
            }
        } else {
            timeout_at(
                self.deadline.min(Instant::now() + OPERATION),
                self.afc.send_op(operation, header, payload),
            )
            .await
        };
        match result {
            Ok(Ok(reply)) => {
                if self.readonly_summary {
                    self.summary_reply(&reply);
                } else if raw_reply {
                    self.evidence_reply(sequence, &reply, reply_reservation)?;
                } else {
                    // Audio/readback payloads are streamed and hashed, never
                    // retained as raw packets in the production evidence log.
                    let mut record = Dictionary::new();
                    record.insert("Opcode".into(), (reply.header.operation as u64).into());
                    record.insert("PacketNumber".into(), reply.header.packet_num.into());
                    record.insert("Header".into(), Value::Data(reply.header_payload.clone()));
                    record.insert("PayloadBytes".into(), (reply.payload.len() as u64).into());
                    record.insert(
                        "PayloadSHA256".into(),
                        hex(&Sha256::digest(&reply.payload)).into(),
                    );
                    self.evidence_reserved_xml(
                        &format!("afc-{sequence:04}-reply.plist"),
                        &Value::Dictionary(record),
                        reply_reservation,
                    )?;
                }
                let expected = match operation {
                    AfcOpcode::GetFileInfo | AfcOpcode::ReadDir | AfcOpcode::Read => {
                        AfcOpcode::Data
                    }
                    AfcOpcode::FileOpen => AfcOpcode::FileOpenRes,
                    AfcOpcode::Write
                    | AfcOpcode::FileSetSize
                    | AfcOpcode::FileClose
                    | AfcOpcode::RenamePath
                    | AfcOpcode::RemovePath
                    | AfcOpcode::MakeDir => AfcOpcode::Status,
                    _ => bail!("unsupported AFC operation in this transport"),
                };
                ensure!(
                    reply.header.operation == expected,
                    "unexpected AFC reply; connection discarded"
                );
                match expected {
                    AfcOpcode::Data => ensure!(
                        reply.header_payload.is_empty(),
                        "unexpected data header; connection discarded"
                    ),
                    AfcOpcode::FileOpenRes => ensure!(
                        reply.header_payload.len() == 8 && reply.payload.is_empty(),
                        "malformed open reply; connection discarded"
                    ),
                    AfcOpcode::Status => ensure!(
                        reply.header_payload == [0; 8] && reply.payload.is_empty(),
                        "malformed status reply; connection discarded"
                    ),
                    _ => unreachable!(),
                }
                self.usable = true;
                Ok(Reply::Packet(reply))
            }
            Ok(Err(error)) => {
                if !self.readonly_summary {
                    let text = if self.output.is_some() {
                        bounded_debug(&error)
                    } else {
                        format!("{error:?}\n")
                    };
                    self.evidence_reserved_bytes(
                        &format!("afc-{sequence:04}-error.txt"),
                        text.as_bytes(),
                        reply_reservation,
                    )?;
                } else if let Some(frame) = self.recent_frames.back_mut() {
                    frame.as_dictionary_mut().unwrap().insert(
                        "StatusOrTransportError".into(),
                        (if self.current_summary || self.output.is_some() {
                            bounded_debug(&error)
                        } else {
                            format!("{error:?}")
                        })
                        .chars()
                        .take(512)
                        .collect::<String>()
                        .into(),
                    );
                }
                self.usable = afc::complete_status(&error);
                if matches!(error, IdeviceError::Afc(AfcError::ObjectNotFound)) {
                    return Ok(Reply::NotFound);
                }
                Err(anyhow::anyhow!(error).context("AFC operation failed; journal retained"))
            }
            Err(_) => {
                if !self.readonly_summary {
                    self.evidence_reserved_bytes(
                        &format!("afc-{sequence:04}-error.txt"),
                        b"Operation timeout; connection discarded, no cleanup I/O\n",
                        reply_reservation,
                    )?;
                }
                bail!("AFC operation timed out; session discarded")
            }
        }
    }

    fn packet(reply: Reply, expected: AfcOpcode) -> Result<AfcPacket> {
        let Reply::Packet(packet) = reply else {
            bail!("unexpected ObjectNotFound")
        };
        ensure!(
            packet.header.operation == expected,
            "unexpected AFC response operation"
        );
        Ok(packet)
    }

    async fn status(&mut self, op: AfcOpcode, header: Vec<u8>, body: Vec<u8>) -> Result<()> {
        let packet = Self::packet(self.op(op, header, body).await?, AfcOpcode::Status)?;
        ensure!(
            packet.header_payload == [0; 8] && packet.payload.is_empty(),
            "invalid AFC success acknowledgement"
        );
        Ok(())
    }

    pub async fn stat(&mut self, path: &str) -> Result<Option<Attributes>> {
        let result = self.stat_inner(path).await;
        self.summary_result(result)
    }

    async fn stat_inner(&mut self, path: &str) -> Result<Option<Attributes>> {
        validate_path(path)?;
        let reply = self
            .op(AfcOpcode::GetFileInfo, path.as_bytes().to_vec(), Vec::new())
            .await?;
        if matches!(reply, Reply::NotFound) {
            return Ok(None);
        }
        attributes_of(Self::packet(reply, AfcOpcode::Data)?).map(Some)
    }

    /// Each path's `stat`. On a plain read-only summary connection a window of
    /// requests is in flight at once (`afc::stat::pipelined`), each still one
    /// operation of the budget with its own summary frame; any failure
    /// discards the connection, as one failed stat does. Other connections
    /// stat lock-step.
    pub async fn stat_all(&mut self, paths: &[&str]) -> Result<Vec<Option<Attributes>>> {
        if !self.readonly_summary || self.current_summary || self.output.is_some() {
            let mut stats = Vec::with_capacity(paths.len());
            for path in paths {
                stats.push(self.stat(path).await?);
            }
            return Ok(stats);
        }
        let result = self.stat_pipelined(paths).await;
        self.summary_result(result)
    }

    async fn stat_pipelined(&mut self, paths: &[&str]) -> Result<Vec<Option<Attributes>>> {
        let mut stats = Vec::with_capacity(paths.len());
        for window in paths.chunks(afc::stat::STAT_WINDOW) {
            ensure!(self.usable, "AFC session already discarded");
            for path in window {
                validate_path(path)?;
            }
            ensure!(
                !self
                    .cancelled
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Relaxed)),
                "AFC operation cancelled; connection discarded"
            );
            ensure!(
                self.sequence + window.len() <= self.operation_limit
                    && Instant::now() < self.deadline,
                "AFC operation/deadline budget exhausted"
            );
            self.usable = false;
            let first = self.sequence;
            self.sequence += window.len();
            let replies = timeout_at(
                self.deadline.min(Instant::now() + OPERATION),
                afc::stat::pipelined(&mut self.afc, window),
            )
            .await;
            let replies = match replies {
                Ok(Ok(replies)) => replies,
                Ok(Err(error)) => {
                    self.summary_frame(first, AfcOpcode::GetFileInfo, window[0].as_bytes());
                    if let Some(frame) = self.recent_frames.back_mut() {
                        frame.as_dictionary_mut().unwrap().insert(
                            "StatusOrTransportError".into(),
                            format!("{error:?}")
                                .chars()
                                .take(512)
                                .collect::<String>()
                                .into(),
                        );
                    }
                    return Err(
                        anyhow::anyhow!(error).context("AFC operation failed; journal retained")
                    );
                }
                Err(_) => bail!("AFC operation timed out; session discarded"),
            };
            for (sequence, (path, reply)) in (first..).zip(window.iter().zip(replies)) {
                self.summary_frame(sequence, AfcOpcode::GetFileInfo, path.as_bytes());
                stats.push(match reply {
                    Some(packet) => {
                        self.summary_reply(&packet);
                        ensure!(
                            packet.header.operation == AfcOpcode::Data,
                            "unexpected AFC reply; connection discarded"
                        );
                        Some(attributes_of(packet)?)
                    }
                    None => {
                        if let Some(frame) = self.recent_frames.back_mut() {
                            frame.as_dictionary_mut().unwrap().insert(
                                "StatusOrTransportError".into(),
                                "Afc(ObjectNotFound)".into(),
                            );
                        }
                        None
                    }
                });
            }
            self.usable = true;
        }
        Ok(stats)
    }

    pub async fn require_absent(&mut self, path: &str) -> Result<()> {
        let result = async {
            ensure!(
                self.stat(path).await?.is_none(),
                "candidate path already exists; nothing removed or overwritten"
            );
            Ok(())
        }
        .await;
        self.summary_result(result)
    }

    pub async fn require_directory(&mut self, path: &str) -> Result<()> {
        let result = async {
            let attrs = self
                .stat(path)
                .await?
                .context("required directory absent")?;
            ensure!(
                attrs.get("st_ifmt").map(String::as_str) == Some("S_IFDIR"),
                "required path is not a directory"
            );
            Ok(())
        }
        .await;
        self.summary_result(result)
    }

    /// A directory, or nothing at all (which holds nothing either).
    pub async fn require_directory_or_absent(&mut self, path: &str) -> Result<()> {
        if self.stat(path).await?.is_some() {
            self.require_directory(path).await?;
        }
        Ok(())
    }

    pub async fn names(&mut self, path: &str) -> Result<Vec<String>> {
        let result = self.names_inner(path).await;
        self.summary_result(result)
    }

    /// The names in a directory that may be absent; absent lists nothing.
    pub async fn names_if_present(&mut self, path: &str) -> Result<Vec<String>> {
        if self.stat(path).await?.is_none() {
            return Ok(Vec::new());
        }
        self.names(path).await
    }

    async fn names_inner(&mut self, path: &str) -> Result<Vec<String>> {
        self.require_directory(path).await?;
        let packet = Self::packet(
            self.op(AfcOpcode::ReadDir, path.as_bytes().to_vec(), Vec::new())
                .await?,
            AfcOpcode::Data,
        )?;
        ensure!(
            packet.header_payload.is_empty(),
            "unexpected directory response header"
        );
        let names = strings(&packet.payload)?;
        ensure!(
            names.len() <= 4096
                && names
                    .iter()
                    .all(|name| name.len() <= 255 && !name.contains('/')),
            "unsupported directory response"
        );
        Ok(names)
    }

    async fn open(&mut self, path: &str, mode: AfcFopenMode) -> Result<u64> {
        validate_path(path)?;
        let mut header = (mode as u64).to_le_bytes().to_vec();
        header.extend_from_slice(path.as_bytes());
        let packet = Self::packet(
            self.op(AfcOpcode::FileOpen, header, Vec::new()).await?,
            AfcOpcode::FileOpenRes,
        )?;
        ensure!(
            packet.header_payload.len() == 8 && packet.payload.is_empty(),
            "invalid AFC open acknowledgement"
        );
        Ok(u64::from_le_bytes(
            packet.header_payload.try_into().unwrap(),
        ))
    }

    async fn close_after<T>(&mut self, handle: u64, result: Result<T>) -> Result<T> {
        if !self.usable {
            return result;
        }
        let close = self
            .status(
                AfcOpcode::FileClose,
                handle.to_le_bytes().to_vec(),
                Vec::new(),
            )
            .await;
        match (result, close) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.context("AFC close unproved")),
            (Err(error), Err(close)) => {
                Err(error.context(format!("AFC close also failed: {close:#}")))
            }
        }
    }

    /// Only safe under continued sole-writer ownership. Rw does not truncate an
    /// existing file. Absence and empty opened path are both required before data.
    pub async fn write_new(
        &mut self,
        path: &str,
        input: &mut impl Read,
        size: u64,
    ) -> Result<[u8; 32]> {
        self.write_bounded("write_new", path, input, size, MAX_ASSET)
            .await
    }

    pub async fn write_audio_new(
        &mut self,
        path: &str,
        input: &mut impl Read,
        size: u64,
    ) -> Result<[u8; 32]> {
        self.write_bounded("write_audio_new", path, input, size, MAX_AUDIO)
            .await
    }

    async fn write_bounded(
        &mut self,
        method: &str,
        path: &str,
        input: &mut impl Read,
        size: u64,
        cap: u64,
    ) -> Result<[u8; 32]> {
        self.require_writable(method)?;
        ensure!(size > 0 && size <= cap, "invalid write size");
        self.require_absent(path).await?;
        let handle = self.open(path, AfcFopenMode::Rw).await?;
        let result = async {
            let attrs = self
                .stat(path)
                .await?
                .context("newly opened path disappeared")?;
            regular_size(&attrs, 0)?;
            let mut remaining = size;
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; CHUNK];
            while remaining > 0 {
                let length = remaining.min(CHUNK as u64) as usize;
                input
                    .read_exact(&mut buffer[..length])
                    .context("source short/read error; partial destination retained")?;
                self.status(
                    AfcOpcode::Write,
                    handle.to_le_bytes().to_vec(),
                    buffer[..length].to_vec(),
                )
                .await?;
                digest.update(&buffer[..length]);
                remaining -= length as u64;
            }
            let args = [handle.to_le_bytes(), size.to_le_bytes()].concat();
            self.status(AfcOpcode::FileSetSize, args, Vec::new())
                .await?;
            Ok(digest.finalize().into())
        }
        .await;
        let digest: [u8; 32] = self.close_after(handle, result).await?;
        let attrs = self
            .stat(path)
            .await?
            .context("completed file disappeared")?;
        regular_size(&attrs, size)?;
        Ok(digest)
    }

    pub async fn read_exact(
        &mut self,
        path: &str,
        expected: u64,
        cap: u64,
        output: &mut impl std::io::Write,
    ) -> Result<[u8; 32]> {
        self.read_bounded(path, expected, cap, output, true).await
    }

    /// `read_exact`, also returning the stats it took just before opening and
    /// just after closing the file, so a caller needs no pair of its own.
    pub async fn read_exact_observed(
        &mut self,
        path: &str,
        expected: u64,
        cap: u64,
        output: &mut impl std::io::Write,
    ) -> Result<ObservedRead> {
        let result = self.read_inner(path, expected, cap, output, true).await;
        self.summary_result(result)
    }

    /// Full streamed readback, no payload log. Default connections retain
    /// per-chunk size/hash evidence; read-only summary connections retain one
    /// per-file hash/attributes receipt instead.
    pub async fn read_hash(&mut self, path: &str, expected: u64, cap: u64) -> Result<[u8; 32]> {
        self.read_bounded(path, expected, cap, &mut std::io::sink(), false)
            .await
    }

    async fn read_bounded(
        &mut self,
        path: &str,
        expected: u64,
        cap: u64,
        output: &mut impl std::io::Write,
        raw_reply: bool,
    ) -> Result<[u8; 32]> {
        let result = self
            .read_inner(path, expected, cap, output, raw_reply)
            .await
            .map(|read| read.sha256);
        self.summary_result(result)
    }

    async fn read_inner(
        &mut self,
        path: &str,
        expected: u64,
        cap: u64,
        output: &mut impl std::io::Write,
        raw_reply: bool,
    ) -> Result<ObservedRead> {
        ensure!(
            expected <= cap && cap <= MAX_CATALOG,
            "invalid read size/cap"
        );
        if self.readonly_summary {
            if self.current_summary {
                ensure!(
                    self.readbacks < CURRENT_READ_RECEIPTS,
                    "Current read receipt allowance exhausted"
                );
            }
            ensure!(self.usable, "AFC session already discarded");
            validate_path(path)?;
            let mut context = Dictionary::new();
            context.insert("Path".into(), path.into());
            context.insert("ExpectedBytes".into(), expected.into());
            context.insert("BytesRead".into(), 0_u64.into());
            context.insert("FirstOperation".into(), (self.sequence as u64).into());
            self.active_read = Some(context);
        }
        let first_operation = self.sequence;
        let before = self.stat(path).await?.ok_or_else(|| ReadbackPathAbsent {
            path: path.to_owned(),
            expected_bytes: expected,
            operation_sequence: first_operation,
        })?;
        regular_size(&before, expected)?;
        let handle = self.open(path, AfcFopenMode::RdOnly).await?;
        let result = async {
            let mut remaining = expected;
            let mut digest = Sha256::new();
            while remaining > 0 {
                let want = remaining.min(CHUNK as u64);
                let header = [handle.to_le_bytes(), want.to_le_bytes()].concat();
                let packet = Self::packet(
                    self.op_evidence(AfcOpcode::Read, header, Vec::new(), raw_reply)
                        .await?,
                    AfcOpcode::Data,
                )?;
                ensure!(
                    packet.header_payload.is_empty()
                        && !packet.payload.is_empty()
                        && packet.payload.len() as u64 <= want,
                    "invalid/short readback chunk"
                );
                output.write_all(&packet.payload)?;
                digest.update(&packet.payload);
                remaining -= packet.payload.len() as u64;
                if let Some(context) = self.active_read.as_mut() {
                    context.insert("BytesRead".into(), (expected - remaining).into());
                }
            }
            Ok(digest.finalize().into())
        }
        .await;
        // A malformed chunk or local sink failure cannot trigger a close or any
        // later operation in summary mode. The detailed legacy mode preserves
        // its existing complete-status cleanup behavior.
        let result = self.summary_result(result);
        let digest: [u8; 32] = self.close_after(handle, result).await?;
        let after = self
            .stat(path)
            .await?
            .context("readback path disappeared")?;
        regular_size(&after, expected)?;
        ensure!(same_file(&before, &after), "readback attributes changed");
        if self.readonly_summary {
            ensure!(
                before
                    .iter()
                    .filter(|(key, _)| key.as_str() != "st_atime")
                    .eq(after.iter().filter(|(key, _)| key.as_str() != "st_atime")),
                "readback attributes changed"
            );
            let mut record = Dictionary::new();
            record.insert("Path".into(), path.into());
            record.insert("Bytes".into(), expected.into());
            record.insert("SHA256".into(), hex(&digest).into());
            record.insert("FirstOperation".into(), (first_operation as u64).into());
            record.insert(
                "Operations".into(),
                ((self.sequence - first_operation) as u64).into(),
            );
            record.insert("BeforeAttributes".into(), attributes_value(&before));
            record.insert("AfterAttributes".into(), attributes_value(&after));
            self.summary_write(&format!("afc-readback-{:04}.plist", self.readbacks), record)?;
            self.readbacks += 1;
            self.active_read = None;
        }
        Ok(ObservedRead {
            sha256: digest,
            before,
            after,
        })
    }

    /// Returns the renamed file's stat.
    pub async fn rename_absent(
        &mut self,
        source: &str,
        target: &str,
        size: u64,
    ) -> Result<Attributes> {
        self.require_writable("rename_absent")?;
        validate_path(source)?;
        validate_path(target)?;
        let attrs = self.stat(source).await?.context("staged source absent")?;
        regular_size(&attrs, size)?;
        self.require_absent(target).await?;
        let args = [source.as_bytes(), &[0], target.as_bytes(), &[0]].concat();
        self.status(AfcOpcode::RenamePath, args, Vec::new()).await?;
        self.require_absent(source).await?;
        let renamed = self.stat(target).await?.context("renamed target absent")?;
        regular_size(&renamed, size)?;
        Ok(renamed)
    }

    /// Remove an operation pair a host staged and never finished. The phone
    /// takes METADATA and SIGNATURE only on FinishedSyncingMetadata, so
    /// without it they stay for good and refuse every later staging. That the
    /// message never went is the caller's to prove (from its own send trace);
    /// this checks the bytes. Each of SIGNATURE, then METADATA, that exists
    /// must be a regular file holding exactly `signature` / `operation`; only
    /// then is that one path removed and its absence confirmed. A locked
    /// connection can do this and remove nothing else. Returns the paths
    /// removed, in order.
    pub async fn remove_staged_operation(
        &mut self,
        operation: &[u8],
        signature: &[u8; 21],
    ) -> Result<Vec<&'static str>> {
        ensure!(
            !self.readonly_summary,
            "removal refused by read-only AFC summary transport"
        );
        let mut removed = Vec::new();
        for (path, expected) in [(SIGNATURE, &signature[..]), (METADATA, operation)] {
            let Some(attrs) = self.stat(path).await? else {
                continue;
            };
            let bytes = expected.len() as u64;
            regular_size(&attrs, bytes)
                .with_context(|| format!("staged {path} is not this operation; nothing removed"))?;
            let read = self.read_hash(path, bytes, bytes).await?;
            ensure!(
                read[..] == Sha256::digest(expected)[..],
                "staged {path} differs from this operation; nothing removed"
            );
            self.checked_removal = Some(path.to_owned());
            let result = self
                .status(AfcOpcode::RemovePath, path.as_bytes().to_vec(), Vec::new())
                .await;
            self.checked_removal = None;
            result?;
            self.require_absent(path).await?;
            removed.push(path);
        }
        Ok(removed)
    }

    /// Create a directory whose parent exists, unless it already exists; an
    /// existing path must be a directory. Nothing is removed or replaced.
    pub async fn ensure_directory(&mut self, path: &str) -> Result<()> {
        self.require_writable("ensure_directory")?;
        validate_path(path)?;
        if self.stat(path).await?.is_none() {
            self.status(AfcOpcode::MakeDir, path.as_bytes().to_vec(), Vec::new())
                .await?;
        }
        self.require_directory(path).await
    }

    /// Every method that creates, writes, renames or makes a directory begins
    /// here, before any request is built or sent.
    fn require_writable(&mut self, method: &str) -> Result<()> {
        let result = if self.readonly_summary {
            Err(anyhow::anyhow!(
                "write/rename refused by read-only AFC summary transport"
            ))
        } else if !self.writable {
            Err(anyhow::anyhow!(
                "{method} refused: no Started permit unlocked this connection"
            ))
        } else {
            Ok(())
        };
        self.summary_result(result)
    }
}

/// Where audio is staged before the phone takes it; covers go in `Artwork`.
pub const STAGING_MEDIA: &str = "Airlock/Media";
pub const STAGING_ARTWORK: &str = "Airlock/Media/Artwork";

/// The phone deletes `Airlock` when it restarts, and guards accept it absent.
/// After Started, before the first upload, its folders are created where
/// missing (never replaced).
pub async fn staging_folders(afc: &mut Transfer, artwork: bool) -> Result<()> {
    afc.ensure_directory("Airlock").await?;
    afc.ensure_directory(STAGING_MEDIA).await?;
    if artwork {
        afc.ensure_directory(STAGING_ARTWORK).await?;
    }
    Ok(())
}

fn attributes_value(attributes: &Attributes) -> Value {
    Value::Dictionary(
        attributes
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// A GetFileInfo reply's attributes.
fn attributes_of(packet: AfcPacket) -> Result<Attributes> {
    ensure!(
        packet.header_payload.is_empty(),
        "unexpected attributes header"
    );
    let parts = strings(&packet.payload)?;
    ensure!(
        parts.len().is_multiple_of(2),
        "odd AFC attribute key/value count"
    );
    let mut attributes = Attributes::new();
    for pair in parts.chunks_exact(2) {
        ensure!(
            attributes
                .insert(pair[0].clone(), pair[1].clone())
                .is_none(),
            "duplicate AFC attribute"
        );
    }
    Ok(attributes)
}

/// Whether a request can create, write, rename or remove anything on the
/// phone: every opcode except stat, list, read and close, and an open in any
/// mode but read-only.
fn mutates(operation: AfcOpcode, header: &[u8]) -> bool {
    match operation {
        AfcOpcode::GetFileInfo | AfcOpcode::ReadDir | AfcOpcode::Read | AfcOpcode::FileClose => {
            false
        }
        AfcOpcode::FileOpen => {
            header.get(..8) != Some(&(AfcFopenMode::RdOnly as u64).to_le_bytes()[..])
        }
        _ => true,
    }
}

/// Defense in depth below every public method of a read-only summary
/// connection: it can only read, stat/list, and close read-only handles.
/// Request bodies are always empty.
fn readonly_operation(operation: AfcOpcode, header: &[u8], payload: &[u8]) -> Result<()> {
    ensure!(payload.is_empty(), "read-only AFC request has payload");
    match operation {
        AfcOpcode::GetFileInfo | AfcOpcode::ReadDir => validate_path(std::str::from_utf8(header)?),
        AfcOpcode::FileOpen => {
            ensure!(
                header.len() >= 8 && header[..8] == (AfcFopenMode::RdOnly as u64).to_le_bytes(),
                "non-read-only AFC open refused"
            );
            validate_path(std::str::from_utf8(&header[8..])?)
        }
        AfcOpcode::Read => {
            ensure!(header.len() == 16, "invalid AFC read header");
            let count = u64::from_le_bytes(header[8..].try_into().unwrap());
            ensure!(
                (1..=CHUNK as u64).contains(&count),
                "AFC read exceeds chunk bound"
            );
            Ok(())
        }
        AfcOpcode::FileClose => {
            ensure!(header.len() == 8, "invalid AFC close header");
            Ok(())
        }
        _ => bail!("mutating/unsupported AFC opcode refused by read-only transport"),
    }
}

/// Whether two stats show the same unchanged file: a rename keeps all of these.
pub fn same_file(before: &Attributes, after: &Attributes) -> bool {
    ["st_size", "st_mtime", "st_birthtime", "st_ifmt"]
        .iter()
        .all(|key| before.get(*key) == after.get(*key))
}

/// A full read's SHA-256, with the file's stat just before it was opened and
/// just after it was closed; the read checked that they agree.
pub struct ObservedRead {
    pub sha256: [u8; 32],
    pub before: Attributes,
    pub after: Attributes,
}

pub fn regular_size(attrs: &Attributes, size: u64) -> Result<()> {
    ensure!(
        attrs.get("st_ifmt").map(String::as_str) == Some("S_IFREG"),
        "not a regular file"
    );
    let actual: u64 = attrs.get("st_size").context("missing file size")?.parse()?;
    ensure!(
        actual == size,
        "unexpected file size: expected {size} bytes, observed {actual} bytes"
    );
    ensure!(
        attrs.contains_key("st_mtime"),
        "missing file modification time"
    );
    Ok(())
}
fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 255
            && !path.starts_with('/')
            && !path.contains(['\0', '\\'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid root-relative AFC path"
    );
    Ok(())
}
fn strings(bytes: &[u8]) -> Result<Vec<String>> {
    ensure!(
        bytes.len() <= 1024 * 1024 && bytes.last() == Some(&0),
        "invalid bounded AFC string payload"
    );
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| Ok(std::str::from_utf8(part)?.to_owned()))
        .collect()
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::{
        Idevice,
        afc::{MAGIC, packet::AfcPacketHeader},
    };
    use std::{
        io::Cursor,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "reference-transfer-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn pair(trace: &Path, duration: Duration) -> (Transfer, Idevice) {
        let (a, b) = tokio::io::duplex(CHUNK * 2);
        (
            Transfer {
                afc: AfcClient::new(Idevice::new(Box::new(a), "offline-only")),
                deadline: Instant::now() + duration,
                trace: trace.to_owned(),
                sequence: 0,
                usable: true,
                operation_limit: MAX_OPS,
                cancelled: None,
                readonly_summary: false,
                current_summary: false,
                recent_frames: VecDeque::new(),
                readbacks: 0,
                failure_recorded: false,
                active_read: None,
                output: None,
                summary_failure: None,
                writable: false,
                checked_removal: None,
            },
            Idevice::new(Box::new(b), "offline-peer"),
        )
    }

    #[tokio::test]
    async fn initial_read_not_found_is_typed_exact_and_discards_without_retry() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        transfer.readonly_summary = true;
        transfer.operation_limit = 1;
        let path = "iTunes_Control/Music/F39/OMJJ.m4a";
        let server = tokio::spawn(async move {
            let req = request(&mut peer, AfcOpcode::GetFileInfo).await;
            assert_eq!(req.header_payload, path.as_bytes());
            assert!(req.payload.is_empty());
            status(&mut peer, &req, 8).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let mut sink = Vec::new();
        let error = transfer
            .read_exact(path, 148305, MAX_AUDIO, &mut sink)
            .await
            .unwrap_err();
        let missing = error.downcast_ref::<ReadbackPathAbsent>().unwrap();
        assert_eq!(missing.path(), path);
        assert_eq!(missing.expected_bytes(), 148305);
        assert_eq!(missing.operation_sequence(), 0);
        assert_eq!(error.to_string(), "readback path absent");
        assert!(sink.is_empty());
        assert_eq!(transfer.sequence, 1);
        assert!(!transfer.usable);
        let later = transfer
            .read_exact(path, 148305, MAX_AUDIO, &mut sink)
            .await
            .unwrap_err();
        assert!(later.downcast_ref::<ReadbackPathAbsent>().is_none());
        assert_eq!(transfer.sequence, 1);
        drop(transfer);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn initial_read_other_status_transport_and_malformed_reply_are_not_absence() {
        for case in ["permission", "busy", "transport", "malformed"] {
            let trace = Temporary::new();
            let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
            transfer.readonly_summary = true;
            let server = tokio::spawn(async move {
                let req = request(&mut peer, AfcOpcode::GetFileInfo).await;
                match case {
                    "permission" => status(&mut peer, &req, 10).await,
                    "busy" => status(&mut peer, &req, 5).await,
                    "malformed" => {
                        reply(
                            &mut peer,
                            &req,
                            AfcOpcode::Data,
                            Vec::new(),
                            b"st_size\0".to_vec(),
                        )
                        .await
                    }
                    "transport" => return,
                    _ => unreachable!(),
                }
                assert!(peer.read_raw(1).await.is_err());
            });
            let mut sink = Vec::new();
            let error = transfer
                .read_exact(
                    "iTunes_Control/Music/F39/OMJJ.m4a",
                    148305,
                    MAX_AUDIO,
                    &mut sink,
                )
                .await
                .unwrap_err();
            assert!(
                error.downcast_ref::<ReadbackPathAbsent>().is_none(),
                "{case}"
            );
            assert!(sink.is_empty());
            assert_eq!(transfer.sequence, 1);
            assert!(!transfer.usable);
            drop(transfer);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn initial_read_control_or_operation_exhaustion_never_reports_absence() {
        for case in ["cancelled", "deadline", "operations"] {
            let trace = Temporary::new();
            let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
            transfer.readonly_summary = true;
            match case {
                "cancelled" => transfer.cancelled = Some(Arc::new(AtomicBool::new(true))),
                "deadline" => transfer.deadline = Instant::now() - Duration::from_millis(1),
                "operations" => transfer.operation_limit = 0,
                _ => unreachable!(),
            }
            let error = transfer
                .read_exact(
                    "iTunes_Control/Music/F39/OMJJ.m4a",
                    148305,
                    MAX_AUDIO,
                    &mut Vec::new(),
                )
                .await
                .unwrap_err();
            assert!(
                error.downcast_ref::<ReadbackPathAbsent>().is_none(),
                "{case}"
            );
            assert_eq!(transfer.sequence, 0);
            assert!(!transfer.usable);
            drop(transfer);
            assert!(peer.read_raw(1).await.is_err());
        }
    }

    #[tokio::test]
    async fn initial_read_present_then_open_not_found_stays_untyped_failure() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        transfer.readonly_summary = true;
        let server = tokio::spawn(async move {
            attributes(&mut peer, 148305).await;
            let req = request(&mut peer, AfcOpcode::FileOpen).await;
            assert_eq!(
                &req.header_payload[..8],
                &(AfcFopenMode::RdOnly as u64).to_le_bytes()
            );
            status(&mut peer, &req, 8).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let error = transfer
            .read_exact(
                "iTunes_Control/Music/F39/OMJJ.m4a",
                148305,
                MAX_AUDIO,
                &mut Vec::new(),
            )
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<ReadbackPathAbsent>().is_none());
        assert_eq!(transfer.sequence, 2);
        assert!(!transfer.usable);
        drop(transfer);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn retained_second_insert_wal_size_change_is_rejected_with_exact_sizes() {
        // A second insert: raw GetFileInfo Data payloads for
        // MediaLibrary.sqlitedb and its WAL. The state saved after the first
        // insert had the same database size, but an 852872-byte WAL. No media
        // read or write is needed to reproduce this fresh-state refusal.
        const DATABASE: &[u8] = b"st_size\x002613248\x00st_blocks\x006272\x00st_nlink\x001\x00st_ifmt\x00S_IFREG\x00st_mtime\x001700000000000000000\x00st_birthtime\x001600000000000000000\x00";
        const WAL: &[u8] = b"st_size\x00861112\x00st_blocks\x001800\x00st_nlink\x001\x00st_ifmt\x00S_IFREG\x00st_mtime\x001700000000000000000\x00st_birthtime\x001600000000000000000\x00";
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        let server = tokio::spawn(async move {
            for (path, payload) in [
                ("iTunes_Control/iTunes/MediaLibrary.sqlitedb", DATABASE),
                ("iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal", WAL),
            ] {
                let req = request(&mut peer, AfcOpcode::GetFileInfo).await;
                assert_eq!(req.header_payload, path.as_bytes());
                assert!(req.payload.is_empty());
                reply(
                    &mut peer,
                    &req,
                    AfcOpcode::Data,
                    Vec::new(),
                    payload.to_vec(),
                )
                .await;
            }
            assert!(peer.read_raw(1).await.is_err());
        });
        let database = transfer
            .stat("iTunes_Control/iTunes/MediaLibrary.sqlitedb")
            .await
            .unwrap()
            .unwrap();
        regular_size(&database, 2_613_248).unwrap();
        let wal = transfer
            .stat("iTunes_Control/iTunes/MediaLibrary.sqlitedb-wal")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wal.get("st_size").unwrap(), "861112");
        let error = regular_size(&wal, 852_872).unwrap_err();
        assert_eq!(transfer.sequence, 2);
        drop(transfer);
        server.await.unwrap();
        assert_eq!(
            error.to_string(),
            "unexpected file size: expected 852872 bytes, observed 861112 bytes"
        );
    }
    async fn request(peer: &mut Idevice, expected: AfcOpcode) -> AfcPacket {
        let packet = AfcPacket::read(peer).await.unwrap();
        assert_eq!(packet.header.operation, expected);
        packet
    }
    async fn reply(
        peer: &mut Idevice,
        request: &AfcPacket,
        op: AfcOpcode,
        header: Vec<u8>,
        payload: Vec<u8>,
    ) {
        let header_len = 40 + header.len() as u64;
        let packet = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_len + payload.len() as u64,
                header_payload_len: header_len,
                packet_num: request.header.packet_num,
                operation: op,
            },
            header_payload: header,
            payload,
        };
        peer.send_raw(&packet.serialize()).await.unwrap();
    }
    async fn status(peer: &mut Idevice, request: &AfcPacket, code: u64) {
        reply(
            peer,
            request,
            AfcOpcode::Status,
            code.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
    }
    async fn attributes(peer: &mut Idevice, size: u64) {
        let req = request(peer, AfcOpcode::GetFileInfo).await;
        let bytes = format!("st_ifmt\0S_IFREG\0st_size\0{size}\0st_mtime\0123\0").into_bytes();
        reply(peer, &req, AfcOpcode::Data, Vec::new(), bytes).await;
    }
    async fn open_fresh(peer: &mut Idevice) {
        let stat = request(peer, AfcOpcode::GetFileInfo).await;
        status(peer, &stat, 8).await;
        let open = request(peer, AfcOpcode::FileOpen).await;
        assert_eq!(&open.header_payload[..8], &2u64.to_le_bytes());
        reply(
            peer,
            &open,
            AfcOpcode::FileOpenRes,
            42u64.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
        attributes(peer, 0).await;
    }
    async fn close(peer: &mut Idevice) {
        let req = request(peer, AfcOpcode::FileClose).await;
        assert_eq!(req.header_payload, 42u64.to_le_bytes());
        status(peer, &req, 0).await;
    }

    #[tokio::test]
    async fn new_write_is_nontruncating_bounded_sets_exact_size_and_closes() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        transfer.unlock_writes();
        let bytes = vec![7; CHUNK + 3];
        let server = tokio::spawn(async move {
            open_fresh(&mut peer).await;
            for length in [CHUNK, 3] {
                let req = request(&mut peer, AfcOpcode::Write).await;
                assert_eq!(req.payload, vec![7; length]);
                status(&mut peer, &req, 0).await;
            }
            let set = request(&mut peer, AfcOpcode::FileSetSize).await;
            assert_eq!(
                set.header_payload,
                [42u64.to_le_bytes(), ((CHUNK + 3) as u64).to_le_bytes()].concat()
            );
            status(&mut peer, &set, 0).await;
            close(&mut peer).await;
            attributes(&mut peer, (CHUNK + 3) as u64).await;
        });
        let hash = transfer
            .write_new(
                "Airlock/Media/123.m4a",
                &mut Cursor::new(&bytes),
                bytes.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(hash.as_slice(), Sha256::digest(&bytes).as_slice());
        server.await.unwrap();
    }

    async fn directory(peer: &mut Idevice) {
        let req = request(peer, AfcOpcode::GetFileInfo).await;
        let bytes = b"st_ifmt\0S_IFDIR\0st_size\x0064\0".to_vec();
        reply(peer, &req, AfcOpcode::Data, Vec::new(), bytes).await;
    }
    async fn absent(peer: &mut Idevice) {
        let req = request(peer, AfcOpcode::GetFileInfo).await;
        status(peer, &req, 8).await;
    }

    #[tokio::test]
    async fn staging_directory_is_created_only_when_absent_and_never_replaces_a_file() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        transfer.unlock_writes();
        let server = tokio::spawn(async move {
            absent(&mut peer).await;
            let make = request(&mut peer, AfcOpcode::MakeDir).await;
            assert_eq!(make.header_payload, b"Airlock/Media");
            assert!(make.payload.is_empty());
            status(&mut peer, &make, 0).await;
            directory(&mut peer).await;
            // Already there: confirmed, not made again.
            directory(&mut peer).await;
            directory(&mut peer).await;
            // A file in the way is refused, not replaced.
            attributes(&mut peer, 3).await;
            attributes(&mut peer, 3).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        transfer.ensure_directory("Airlock/Media").await.unwrap();
        transfer.ensure_directory("Airlock/Media").await.unwrap();
        let error = transfer.ensure_directory("Airlock").await.unwrap_err();
        assert!(error.to_string().contains("not a directory"));
        drop(transfer);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn absent_staging_directory_passes_guards_and_lists_nothing() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        let server = tokio::spawn(async move {
            absent(&mut peer).await;
            absent(&mut peer).await;
            directory(&mut peer).await;
            directory(&mut peer).await;
            attributes(&mut peer, 3).await;
            attributes(&mut peer, 3).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        transfer
            .require_directory_or_absent("Airlock/Media")
            .await
            .unwrap();
        assert!(
            transfer
                .names_if_present("Airlock/Media/Artwork")
                .await
                .unwrap()
                .is_empty()
        );
        transfer
            .require_directory_or_absent("Airlock/Media")
            .await
            .unwrap();
        assert!(
            transfer
                .require_directory_or_absent("Airlock/Media")
                .await
                .is_err()
        );
        drop(transfer);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn existing_path_or_non_notfound_status_never_opens_or_writes() {
        for status_code in [0, 10] {
            let trace = Temporary::new();
            let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(1));
            transfer.unlock_writes();
            let server = tokio::spawn(async move {
                if status_code == 0 {
                    attributes(&mut peer, 7).await;
                } else {
                    let req = request(&mut peer, AfcOpcode::GetFileInfo).await;
                    status(&mut peer, &req, status_code).await;
                }
                assert!(peer.read_raw(1).await.is_err());
            });
            assert!(
                transfer
                    .write_new("Airlock/Media/123", &mut Cursor::new(b"abc"), 3)
                    .await
                    .is_err()
            );
            assert_eq!(transfer.sequence, 1);
            drop(transfer);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn complete_write_error_closes_but_malformed_reply_discards_without_close() {
        for malformed in [false, true] {
            let trace = Temporary::new();
            let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(1));
            transfer.unlock_writes();
            let server = tokio::spawn(async move {
                open_fresh(&mut peer).await;
                let req = request(&mut peer, AfcOpcode::Write).await;
                if malformed {
                    reply(&mut peer, &req, AfcOpcode::Data, Vec::new(), Vec::new()).await;
                } else {
                    status(&mut peer, &req, 5).await;
                    close(&mut peer).await;
                }
                assert!(peer.read_raw(1).await.is_err());
            });
            assert!(
                transfer
                    .write_new("Airlock/Media/123", &mut Cursor::new(b"abc"), 3)
                    .await
                    .is_err()
            );
            assert_eq!(transfer.usable, !malformed);
            drop(transfer);
            server.await.unwrap();
        }
    }

    // Only the peer's withheld Close reply should consume the test's deadline;
    // synchronous trace writes must not turn this into a pre-Close timeout.
    #[tokio::test(start_paused = true)]
    async fn lost_or_timed_out_close_discards_connection_without_retry() {
        for timeout in [false, true] {
            let trace = Temporary::new();
            let duration = if timeout {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(1)
            };
            let (mut transfer, mut peer) = pair(&trace.0, duration);
            transfer.unlock_writes();
            let server = tokio::spawn(async move {
                open_fresh(&mut peer).await;
                let req = request(&mut peer, AfcOpcode::Write).await;
                status(&mut peer, &req, 0).await;
                let req = request(&mut peer, AfcOpcode::FileSetSize).await;
                status(&mut peer, &req, 0).await;
                request(&mut peer, AfcOpcode::FileClose).await;
                if timeout {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    assert!(peer.read_raw(1).await.is_err());
                }
            });
            assert!(
                transfer
                    .write_new("Airlock/Media/123", &mut Cursor::new(b"abc"), 3)
                    .await
                    .is_err()
            );
            assert!(!transfer.usable);
            assert!(transfer.require_absent("any/path").await.is_err());
            drop(transfer);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn audio_over_32_mib_streams_with_a_separate_64_mib_cap() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(20));
        transfer.unlock_writes();
        let size = MAX_ASSET + 1;
        assert!(
            transfer
                .write_new("Airlock/Media/123.m4a", &mut std::io::empty(), size)
                .await
                .is_err()
        );
        assert!(
            transfer
                .write_audio_new(
                    "Airlock/Media/123.m4a",
                    &mut std::io::empty(),
                    MAX_AUDIO + 1
                )
                .await
                .is_err()
        );
        assert_eq!(transfer.sequence, 0);
        let server = tokio::spawn(async move {
            open_fresh(&mut peer).await;
            let mut remaining = size;
            let mut digest = Sha256::new();
            while remaining > 0 {
                let req = request(&mut peer, AfcOpcode::Write).await;
                assert_eq!(req.payload.len() as u64, remaining.min(CHUNK as u64));
                assert!(req.payload.iter().all(|byte| *byte == 7));
                remaining -= req.payload.len() as u64;
                digest.update(&req.payload);
                status(&mut peer, &req, 0).await;
            }
            let set = request(&mut peer, AfcOpcode::FileSetSize).await;
            assert_eq!(
                set.header_payload,
                [42_u64.to_le_bytes(), size.to_le_bytes()].concat()
            );
            status(&mut peer, &set, 0).await;
            close(&mut peer).await;
            attributes(&mut peer, size).await;
            <[u8; 32]>::from(digest.finalize())
        });
        let hash = transfer
            .write_audio_new(
                "Airlock/Media/123.m4a",
                &mut std::io::repeat(7).take(size),
                size,
            )
            .await
            .unwrap();
        assert_eq!(hash, server.await.unwrap());
    }

    #[tokio::test]
    async fn streamed_readback_hashes_chunks_without_audio_payload_files() {
        let trace = Temporary::new();
        let (mut transfer, mut peer) = pair(&trace.0, Duration::from_secs(2));
        let source = vec![0xa5; CHUNK + 3];
        let expected: [u8; 32] = Sha256::digest(&source).into();
        let server = tokio::spawn(async move {
            attributes(&mut peer, source.len() as u64).await;
            let open = request(&mut peer, AfcOpcode::FileOpen).await;
            assert_eq!(&open.header_payload[..8], &1_u64.to_le_bytes());
            reply(
                &mut peer,
                &open,
                AfcOpcode::FileOpenRes,
                42_u64.to_le_bytes().to_vec(),
                Vec::new(),
            )
            .await;
            for chunk in source.chunks(CHUNK) {
                let read = request(&mut peer, AfcOpcode::Read).await;
                reply(
                    &mut peer,
                    &read,
                    AfcOpcode::Data,
                    Vec::new(),
                    chunk.to_vec(),
                )
                .await;
            }
            close(&mut peer).await;
            attributes(&mut peer, source.len() as u64).await;
        });
        assert_eq!(
            transfer
                .read_hash(
                    "iTunes_Control/Music/F39/TEST.m4a",
                    (CHUNK + 3) as u64,
                    MAX_AUDIO
                )
                .await
                .unwrap(),
            expected
        );
        server.await.unwrap();
        assert!(trace.0.join("afc-0002-reply.plist").is_file());
        assert!(!trace.0.join("afc-0002-reply.bin").exists());
        for entry in std::fs::read_dir(&trace.0).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            assert!(!bytes.windows(64).any(|window| window == [0xa5; 64]));
        }
    }

    #[tokio::test]
    async fn cancellation_and_fixed_budget_stop_before_new_io() {
        for cancelled in [false, true] {
            let trace = Temporary::new();
            let (mut transfer, _peer) = pair(&trace.0, Duration::from_secs(1));
            if cancelled {
                transfer.cancelled = Some(Arc::new(AtomicBool::new(true)));
            } else {
                transfer.operation_limit = 0;
            }
            assert!(
                transfer
                    .stat("iTunes_Control/iTunes/iTunesPrefs")
                    .await
                    .is_err()
            );
            assert!(!transfer.usable);
            assert_eq!(transfer.sequence, 0);
            assert!(std::fs::read_dir(&trace.0).unwrap().next().is_none());
        }
    }

    mod bounded;
    mod locked;
    mod summary;
}
