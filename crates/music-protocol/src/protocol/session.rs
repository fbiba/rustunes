//! Bounded one-track sync session with fixed protocol guards.
//! No generic send API is exposed. Unknown commands or failures end the caller's session.
use super::trace::private_write;
use super::{framing as atc, signing as grappa, trace::OutputAllowance};
use anyhow::{Context, Result, bail, ensure};
use idevice::{Idevice, IdeviceError, IdeviceService, ReadWrite, provider::IdeviceProvider};
use plist::{Dictionary, Value};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context as TaskContext, Poll, ready},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    time::{Instant, timeout_at},
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_FRAMES: usize = 1024;

mod pending_completion;
pub use pending_completion::{
    PendingBodyContext, PendingCompletionEvidence, PendingCompletionFailure,
    PendingCompletionReceipt, PendingPartialReceivedObservation, PendingReceivedObservation,
    PendingWriteObservation,
};
mod stage;
pub use stage::StartedOperation;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetadataMode {
    RevisionDelta,
    EmptyDelta,
}

pub struct Prepared {
    request: Value,
    media_anchor: String,
    finished: Value,
    metadata_mode: MetadataMode,
}

/// Checked message shape for a fresh matching-anchor Media delta with no
/// metadata operations. It does not establish application execution authority.
/// Private custody prevents conversion into an ordinary signing preparation.
pub struct EmptyDeltaPrepared {
    prepared: Prepared,
}

impl EmptyDeltaPrepared {
    pub fn request(&self) -> &Value {
        self.prepared.request()
    }
}
impl Prepared {
    pub fn request(&self) -> &Value {
        &self.request
    }
    pub fn next_anchor(&self) -> Result<u32> {
        ensure!(
            self.metadata_mode == MetadataMode::RevisionDelta,
            "empty delta cannot advance the metadata anchor"
        );
        self.media_anchor
            .parse::<u32>()?
            .checked_add(1)
            .context("Media anchor exhausted")
    }
    fn finished_message(&self) -> Result<Value> {
        let mut value = self.finished.clone();
        let root = value.as_dictionary_mut().unwrap();
        root.insert("Session".into(), 2u64.into());
        root.get_mut("Params")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .get_mut("DataclassAnchors")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("Media".into(), self.next_anchor()?.to_string().into());
        Ok(value)
    }

    fn empty_delta_finished_message(&self) -> Result<Value> {
        ensure!(
            self.metadata_mode == MetadataMode::EmptyDelta,
            "empty metadata finish requires its distinct preparation"
        );
        let mut value = self.finished.clone();
        let root = value.as_dictionary_mut().unwrap();
        root.insert("Session".into(), 2u64.into());
        root.get_mut("Params")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .get_mut("DataclassAnchors")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("Media".into(), self.media_anchor.clone().into());
        Ok(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    MetadataSent,
    DynamicManifestWaiting,
    PendingDemandObserved,
    MetadataOnlyWaiting,
    AssetRequested(AssetPart),
    CompleteSent,
    Finished,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AssetPart {
    Audio = 1,
    Artwork = 2,
}

/// A reviewed metadata-only phase may decline only these exact prior ancillary
/// requests. Construction is offline; it establishes no ownership or permission
/// to mutate the target track. The caller binds IDs to its prepared evidence.
pub struct MetadataOnlyPolicy {
    target: String,
    ancillary: [(String, u64); 2],
}

impl MetadataOnlyPolicy {
    pub fn new(target: &str, ancillary: [(String, u64); 2]) -> Result<Self> {
        fn canonical_id(id: &str) -> bool {
            id.parse::<i64>()
                .is_ok_and(|value| value != 0 && value.to_string() == id)
        }
        ensure!(
            canonical_id(target),
            "invalid metadata-only target identity"
        );
        ensure!(
            ancillary
                .iter()
                .all(|(id, part)| canonical_id(id) && id != target && [2, 4].contains(part))
                && ancillary[0].0 != ancillary[1].0
                && ancillary[0].1 != ancillary[1].1,
            "metadata-only policy requires two distinct foreign artwork/Genius identities"
        );
        Ok(Self {
            target: target.to_owned(),
            ancillary,
        })
    }
}

pub struct Session {
    channel: Channel,
    phase: Phase,
    signer: grappa::FixedHostSession,
    prepared: Prepared,
    /// Own assets requested and not yet completed, in request order.
    assets: Vec<String>,
    ancillary: Vec<String>,
    ready_receipt_sha256: [u8; 32],
}

/// A received command category only. None of these variants grants asset,
/// body, completion, terminal-success or parent-operation authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyDeltaResponseKind {
    AssetManifest,
    SyncFinished,
    SyncFailed,
    UnsupportedCommand,
}

/// Exact bounded received message after a real matching-anchor empty finish.
/// The transport is consumed and closed before this value reaches its caller.
/// Full raw/XML traces remain the evidence; this is not a delivery permit.
pub struct EmptyDeltaObservation {
    kind: EmptyDeltaResponseKind,
    message: Value,
    frame_index: usize,
    frame_sha256: [u8; 32],
    telemetry_messages: usize,
}

impl EmptyDeltaObservation {
    pub fn kind(&self) -> EmptyDeltaResponseKind {
        self.kind
    }
    pub fn message(&self) -> &Value {
        &self.message
    }
    pub fn frame_index(&self) -> usize {
        self.frame_index
    }
    pub fn frame_sha256(&self) -> [u8; 32] {
        self.frame_sha256
    }
    pub fn telemetry_messages(&self) -> usize {
        self.telemetry_messages
    }
}

/// A validated fresh demand with its original live transport held privately.
/// It grants no source, journal or body authority and has no ordinary Session
/// conversion. The distinct consuming callback completion mechanism belongs
/// inside the app's authorized child continuation. Drop closes the transport;
/// this live demand is neither serializable nor cloneable.
pub struct PendingAudioDemand {
    observation: EmptyDeltaObservation,
    manifest: super::ancillary::PendingAudioManifest,
    own_asset_id: String,
    ready_receipt_sha256: [u8; 32],
    original_control: super::transfer::CaptureControl,
    _session: Session,
}
impl PendingAudioDemand {
    pub fn observation(&self) -> &EmptyDeltaObservation {
        &self.observation
    }
    pub fn own_asset_id(&self) -> &str {
        &self.own_asset_id
    }
    pub fn own_parts(&self) -> u64 {
        self.manifest.own_parts()
    }
    pub fn foreign_asset_ids(&self) -> &[String] {
        self.manifest.foreign_asset_ids()
    }
    pub fn ready_receipt_sha256(&self) -> [u8; 32] {
        self.ready_receipt_sha256
    }
}

/// Fresh empty-delta transport. The constructor independently matches Ready
/// through the normal bounded handshake. This API exposes no ordinary signer,
/// metadata staging, asset response, terminal success or inner Session.
/// Native empty-delta acceptance and pending-audio delivery remain unqualified.
pub struct EmptyDeltaSession {
    session: Session,
}

impl EmptyDeltaSession {
    pub fn ready_receipt_sha256(&self) -> [u8; 32] {
        self.session.ready_receipt_sha256()
    }
}

struct Channel {
    client: atc::Atc,
    trace: PathBuf,
    deadline: Instant,
    frames: usize,
    last_received_sha256: [u8; 32],
}

impl Channel {
    async fn send(&mut self, value: &Value) -> Result<()> {
        ensure!(
            Instant::now() < self.deadline,
            "ATC deadline expired before send"
        );
        ensure!(self.frames < MAX_FRAMES, "ATC frame count exhausted");
        timeout_at(
            self.deadline,
            self.client.send_plist(value, &self.trace, self.frames),
        )
        .await
        .context("ATC send deadline; intended frame retained")??;
        self.frames += 1;
        Ok(())
    }
}

impl Session {
    pub fn sign_metadata(&self, exact_bytes: &[u8]) -> Result<[u8; 21]> {
        ensure!(self.phase == Phase::Ready, "signing outside Ready state");
        ensure!(
            self.prepared.metadata_mode == MetadataMode::RevisionDelta,
            "ordinary metadata signing is forbidden for an empty delta"
        );
        self.signer.sign(exact_bytes)
    }

    pub fn ready_receipt_sha256(&self) -> [u8; 32] {
        self.ready_receipt_sha256
    }

    pub fn terminal_receipt_sha256(&self) -> Result<[u8; 32]> {
        ensure!(
            self.phase == Phase::Finished,
            "terminal evidence requested before matching SyncFinished"
        );
        Ok(self.channel.last_received_sha256)
    }
}
