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
}
