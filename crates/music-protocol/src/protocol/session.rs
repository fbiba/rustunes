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
    /// Genuine matching-anchor empty-delta session using the same evidence
    /// allowance through observation and both pending completion halves.
    pub async fn connect_with_output(
        provider: &dyn IdeviceProvider,
        prepared: EmptyDeltaPrepared,
        trace: &Path,
        control: &super::transfer::CaptureControl,
        output: OutputAllowance,
    ) -> Result<Self> {
        Ok(Self {
            session: Session::connect_with_output(
                provider,
                prepared.prepared,
                trace,
                control,
                output,
            )
            .await?,
        })
    }

    pub async fn connect(
        provider: &dyn IdeviceProvider,
        prepared: EmptyDeltaPrepared,
        trace: &Path,
        deadline: Instant,
    ) -> Result<Self> {
        Ok(Self {
            session: Session::connect(provider, prepared.prepared, trace, deadline).await?,
        })
    }

    pub fn ready_receipt_sha256(&self) -> [u8; 32] {
        self.session.ready_receipt_sha256()
    }

    /// Send the real Delta1 Finished message at the independently matched
    /// Request/Ready anchor. The caller separately proves no metadata was staged.
    /// A successful send is an outgoing observation, not receiver completion.
    pub async fn finish_empty_metadata(&mut self) -> Result<()> {
        ensure!(
            self.session.phase == Phase::Ready,
            "empty metadata-finished outside Ready state"
        );
        let finished = self.session.prepared.empty_delta_finished_message()?;
        self.session.send(&finished).await?;
        self.session.phase = Phase::MetadataSent;
        Ok(())
    }
    /// Consume this transport after its real empty metadata finish. Receive
    /// at most 64 further frames, checking existing telemetry until the first
    /// non-telemetry command. No reply or asset/terminal phase transition occurs.
    /// EOF, malformed data, wrong session, deadline or frame exhaustion is an
    /// incomplete capture; existing complete/partial raw traces are retained.
    pub async fn observe_after_empty_metadata(mut self) -> Result<EmptyDeltaObservation> {
        self.receive_empty_observation(None).await
    }

    /// Consume the checked empty session and validate demand in this exact
    /// matching-Ready transport. The result retains the transport privately.
    /// It exposes no source, signer, second metadata finish or ordinary terminal
    /// API; its consuming callback completion needs app-owned child authority.
    /// Any receive/policy error drops the only connection; no fallback exists.
    pub async fn observe_pending_audio(
        mut self,
        policy: &super::ancillary::PendingAudioPolicy<'_>,
        control: &super::transfer::CaptureControl,
    ) -> Result<PendingAudioDemand> {
        control.check()?;
        self.session.channel.client.ensure_output_control(control)?;
        // One absolute phase deadline; telemetry/read loops never renew it.
        let phase_control = super::transfer::CaptureControl {
            deadline: control
                .deadline
                .min(self.session.channel.deadline)
                .min(Instant::now() + Duration::from_secs(35)),
            cancelled: std::sync::Arc::clone(&control.cancelled),
        };
        let host = child(
            child(dictionary(self.session.prepared.request())?, "Params")?,
            "HostInfo",
        )?;
        policy.ensure_session(
            string(host, "LibraryID")?,
            self.session.prepared.media_anchor.parse()?,
        )?;
        let observation = self.receive_empty_observation(Some(&phase_control)).await?;
        phase_control.check()?;
        ensure!(
            observation.kind == EmptyDeltaResponseKind::AssetManifest,
            "pending audio requires an actual manifest; no reply or fallback"
        );
        let manifest = policy.validate_manifest(observation.message(), 2)?;
        // This observation-only phase cannot satisfy complete_asset's ordinary
        // AssetRequested guard, even inside this module.
        self.session.phase = Phase::PendingDemandObserved;
        Ok(PendingAudioDemand {
            observation,
            manifest,
            own_asset_id: policy.own_asset_id().to_owned(),
            ready_receipt_sha256: self.session.ready_receipt_sha256(),
            // Preserve original control independently of the observation-only
            // 35-second cap. Completion never receives a replacement deadline.
            original_control: super::transfer::CaptureControl {
                deadline: control.deadline.min(self.session.channel.deadline),
                cancelled: std::sync::Arc::clone(&control.cancelled),
            },
            _session: self.session,
        })
    }

    async fn receive_empty_observation(
        &mut self,
        control: Option<&super::transfer::CaptureControl>,
    ) -> Result<EmptyDeltaObservation> {
        ensure!(
            self.session.phase == Phase::MetadataSent
                && self.session.prepared.metadata_mode == MetadataMode::EmptyDelta,
            "empty response observation requires its real unchanged-anchor finish"
        );
        for telemetry_messages in 0..64 {
            let frame_index = self.session.channel.frames;
            let value = match control {
                Some(control) => self.session.channel.receive_controlled(control).await?,
                None => self.session.channel.receive().await?,
            };
            let frame_sha256 = self.session.channel.last_received_sha256;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session empty response"
            );
            let kind = match string(root, "Command")? {
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?;
                    continue;
                }
                "AssetManifest" => {
                    // Preserve the actual full dictionary without interpreting
                    // IDs/parts as eligibility or invoking any body policy.
                    child(root, "Params")?;
                    EmptyDeltaResponseKind::AssetManifest
                }
                "SyncFinished" => {
                    if root.contains_key("Params") {
                        child(root, "Params")?;
                    }
                    EmptyDeltaResponseKind::SyncFinished
                }
                "SyncFailed" => {
                    child(root, "Params")?;
                    EmptyDeltaResponseKind::SyncFailed
                }
                _ => {
                    // No authoritative AudioIndex command is presently mapped.
                    // Unknown actual command/Params survive verbatim, no reply.
                    // Its opaque Params may be data/array; no schema is invented.
                    EmptyDeltaResponseKind::UnsupportedCommand
                }
            };
            return Ok(EmptyDeltaObservation {
                kind,
                message: value,
                frame_index,
                frame_sha256,
                telemetry_messages,
            });
        }
        bail!("empty response observation frame bound; no further sends")
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

    /// Pending-only cooperative receive. The operation borrows this Channel,
    /// so outer control rejection cannot drop the owner of consumed fragments.
    /// Completed raw/XML is retained by receive before run's post-check; an
    /// interrupted receive flushes only actual buffered bytes before returning.
    async fn receive_controlled(
        &mut self,
        control: &super::transfer::CaptureControl,
    ) -> Result<Value> {
        let mut inner_returned = false;
        let result = {
            control
                .run(
                    Duration::from_secs(35),
                    "pending audio receive deadline",
                    async {
                        let result = self.receive().await;
                        inner_returned = true;
                        result
                    },
                )
                .await
        }; // The borrowed operation has been dropped before accessing Channel.
        if result.is_err() && !inner_returned && !self.client.partial_frame().is_empty() {
            self.client
                .retain_partial(&self.trace.join(format!("{:03}-partial.atc", self.frames)))?;
        }
        // Inner errors have already attempted existing exclusive retention.
        // Never duplicate that receipt, reuse the connection or return a demand.
        result
    }

    /// The next message. A Ping from the phone is answered with a Pong in its
    /// session, as the phone answers ours, and is not returned: it pinged the
    /// host after the last FileComplete of an 8-track batch while it imported
    /// the files. Each Ping and Pong is traced and counts toward
    /// the frame bound.
    async fn receive(&mut self) -> Result<Value> {
        loop {
            let result = self.receive_inner().await;
            if result.is_err() {
                self.client.discard_bounded();
            }
            let value = result?;
            let root = dictionary(&value)?;
            if string(root, "Command")? != "Ping" {
                return Ok(value);
            }
            let mut pong = Dictionary::new();
            pong.insert("Session".into(), number(root, "Session")?.into());
            pong.insert("Command".into(), "Pong".into());
            self.send(&Value::Dictionary(pong)).await?;
        }
    }

    async fn receive_inner(&mut self) -> Result<Value> {
        ensure!(
            Instant::now() < self.deadline,
            "ATC deadline expired before receive"
        );
        ensure!(self.frames < MAX_FRAMES, "ATC frame count exhausted");
        let partial_path = self.trace.join(format!("{:03}-partial.atc", self.frames));
        let evidence_limit = self
            .client
            .reserve_received(&partial_path)?
            .unwrap_or(MAX_BYTES + 4);
        let result = timeout_at(
            self.deadline,
            self.client.read_frame_with_limit(evidence_limit),
        )
        .await;
        let frame = match result {
            Ok(Ok(frame)) => frame,
            other => {
                if !self.client.partial_frame().is_empty() {
                    self.client.retain_partial(&partial_path)?;
                }
                match other {
                    Ok(Err(error)) => {
                        return Err(error.context("ATC read failed; partial retained"));
                    }
                    Err(_) => bail!("ATC read deadline; partial retained"),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        };
        let raw_path = self.trace.join(format!("{:03}-received.atc", self.frames));
        self.client.retain_received(&raw_path, &frame)?;
        self.last_received_sha256 = Sha256::digest(&frame).into();
        let value: Value = plist::from_bytes(&frame[4..]).context("invalid incoming plist")?;
        self.client.retain_xml(
            &self
                .trace
                .join(format!("{:03}-received.plist", self.frames)),
            &value,
        )?;
        self.frames += 1;
        let root = dictionary(&value)?;
        let command = string(root, "Command")?;
        let without_params = matches!(command, "Pong" | "Ping")
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
            "unsupported incoming message type"
        );
        number(root, "Id")?;
        number(root, "Session")?;
        Ok(value)
    }
}

impl Session {
    /// Ordinary metadata session whose outgoing, incoming and interrupted
    /// evidence shares the caller's aggregate output allowance. This is not a
    /// whole-step storage reservation and grants no execution authority.
    pub async fn connect_with_output(
        provider: &dyn IdeviceProvider,
        prepared: Prepared,
        trace: &Path,
        control: &super::transfer::CaptureControl,
        output: OutputAllowance,
    ) -> Result<Self> {
        control.check()?;
        let deadline = control.deadline;
        let handshake_deadline = deadline.min(Instant::now() + Duration::from_secs(30));
        let client = control
            .run(
                Duration::from_secs(30),
                "bounded-output ATC connect",
                async { Ok(SessionTransport::connect(provider).await?.0) },
            )
            .await?
            .with_output(output, control)?;
        Self::begin(client, prepared, trace, deadline, handshake_deadline).await
    }

    pub async fn connect(
        provider: &dyn IdeviceProvider,
        prepared: Prepared,
        trace: &Path,
        deadline: Instant,
    ) -> Result<Self> {
        let handshake_deadline = deadline.min(Instant::now() + Duration::from_secs(30));
        ensure!(
            Instant::now() < handshake_deadline,
            "ATC deadline expired before connect"
        );
        let client = timeout_at(handshake_deadline, SessionTransport::connect(provider))
            .await
            .context("ATC connection deadline")??
            .0;
        Self::begin(client, prepared, trace, deadline, handshake_deadline).await
    }

    async fn begin(
        client: atc::Atc,
        prepared: Prepared,
        trace: &Path,
        deadline: Instant,
        handshake_deadline: Instant,
    ) -> Result<Self> {
        let mut channel = Channel {
            client,
            trace: trace.to_owned(),
            deadline: handshake_deadline,
            frames: 0,
            last_received_sha256: [0; 32],
        };
        let mut ping = Dictionary::new();
        ping.insert("Session".into(), 1u64.into());
        ping.insert("Command".into(), "Ping".into());
        channel.send(&Value::Dictionary(ping)).await?;
        let (mut pong, mut capable, mut allowed, mut requested) = (false, false, false, false);
        while channel.frames < 64 {
            let value = channel.receive().await?;
            let root = dictionary(&value)?;
            match string(root, "Command")? {
                "Pong" => {
                    ensure!(!pong && !requested, "duplicate/late Pong");
                    pong = true;
                }
                "Capabilities" => {
                    let params = child(root, "Params")?;
                    exact_keys(params, &["GrappaSupportInfo"])?;
                    let info = child(params, "GrappaSupportInfo")?;
                    exact_keys(info, &["version", "deviceType", "protocolVersion"])?;
                    ensure!(
                        number(info, "version")? == 1
                            && number(info, "deviceType")? == 0
                            && number(info, "protocolVersion")? == 1,
                        "unsupported Grappa capabilities"
                    );
                    capable = true;
                }
                "SyncAllowed" => {
                    check_allowed(child(root, "Params")?)?;
                    allowed = true;
                }
                "InstalledAssets" | "AssetMetrics" => {
                    child(root, "Params")?;
                }
                "ReadyForSync" => {
                    ensure!(
                        requested && number(root, "Session")? == 2,
                        "unmatched Ready session"
                    );
                    let params = child(root, "Params")?;
                    exact_keys(params, &["DataclassAnchors", "DeviceInfo"])?;
                    let anchors = child(params, "DataclassAnchors")?;
                    exact_keys(anchors, &["Media", "Book"])?;
                    anchor(anchors, "Book")?;
                    ensure!(
                        string(anchors, "Media")? == prepared.media_anchor,
                        "Ready anchor differs from saved state"
                    );
                    let info = child(params, "DeviceInfo")?;
                    exact_keys(info, &["Grappa"])?;
                    let reply = info
                        .get("Grappa")
                        .and_then(Value::as_data)
                        .context("missing Ready Grappa")?;
                    let signer =
                        grappa::FixedHostSession::from_reply(&grappa::HOST_REQUEST, reply)?;
                    channel.deadline = deadline;
                    let ready_receipt_sha256 = channel.last_received_sha256;
                    return Ok(Self {
                        channel,
                        phase: Phase::Ready,
                        signer,
                        prepared,
                        assets: Vec::new(),
                        ancillary: Vec::new(),
                        ready_receipt_sha256,
                    });
                }
                _ => bail!("unexpected handshake command; no further sends"),
            }
            if !requested && pong && capable && allowed {
                channel.send(&prepared.request).await?;
                requested = true;
            }
        }
        bail!("handshake frame limit reached")
    }

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

    async fn send(&mut self, value: &Value) -> Result<()> {
        self.channel.send(value).await
    }

    pub async fn finish_metadata(&mut self) -> Result<()> {
        ensure!(
            self.phase == Phase::Ready,
            "metadata-finished outside Ready state"
        );
        self.send(&self.prepared.finished_message()?).await?;
        self.phase = Phase::MetadataSent;
        Ok(())
    }

    pub async fn wait_for_audio(
        &mut self,
        expected_asset_id: &str,
        allowed_ancillary: Option<&[(String, u64); 2]>,
    ) -> Result<()> {
        self.wait_for_asset(expected_asset_id, AssetPart::Audio, allowed_ancillary)
            .await
    }

    /// Production audio-only path. Eligibility is bound to the engine's fresh
    /// catalog, while IDs/parts are taken exclusively from this session's full
    /// manifest. Consume the wait even on error so callers cannot recover by
    /// switching to a less restrictive legacy method in the same session.
    pub async fn wait_for_audio_with_policy(
        &mut self,
        policy: &super::ancillary::AudioPolicy<'_>,
    ) -> Result<()> {
        self.wait_for_asset_with_policy(policy, AssetPart::Audio)
            .await
    }

    /// Separately qualified existing-track artwork-only path. It cannot accept
    /// own audio or make a new-track audio policy authorize an artwork target.
    pub async fn wait_for_artwork_with_policy(
        &mut self,
        policy: &super::ancillary::ArtworkPolicy<'_>,
    ) -> Result<()> {
        self.wait_for_asset_with_policy(policy, AssetPart::Artwork)
            .await
    }

    async fn wait_for_asset_with_policy<const PART: u64>(
        &mut self,
        policy: &super::ancillary::AssetPolicy<'_, PART>,
        part: AssetPart,
    ) -> Result<()> {
        ensure!(
            self.phase == Phase::MetadataSent,
            "manifest wait outside metadata state"
        );
        self.phase = Phase::DynamicManifestWaiting;
        let host = child(
            child(dictionary(self.prepared.request())?, "Params")?,
            "HostInfo",
        )?;
        policy.ensure_session(
            string(host, "LibraryID")?,
            self.prepared.media_anchor.parse()?,
        )?;
        loop {
            let value = self.channel.receive().await?;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session post-metadata message"
            );
            match string(root, "Command")? {
                "AssetManifest" => {
                    // No state retaining any accepted prefix changes until
                    // every record has passed the snapshot-bound policy.
                    let ancillary = policy.validate_manifest(&value, 2)?;
                    self.ancillary = ancillary;
                    self.assets = policy.own_asset_ids().to_vec();
                    self.phase = Phase::AssetRequested(part);
                    return Ok(());
                }
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?
                }
                _ => {
                    bail!("unexpected terminal/request before own asset manifest; no further sends")
                }
            }
        }
    }

    pub async fn wait_for_artwork(
        &mut self,
        expected_asset_id: &str,
        allowed_ancillary: Option<&[(String, u64); 2]>,
    ) -> Result<()> {
        self.wait_for_asset(expected_asset_id, AssetPart::Artwork, allowed_ancillary)
            .await
    }

    /// Wait after metadata staging for a title update/restoration or ordinary
    /// delete. No own asset is expected and no FileComplete is synthesized.
    /// The wait is consumed even on error: close the session and reconcile;
    /// do not call another wait method to continue after an unexpected request.
    pub async fn wait_metadata_only(&mut self, policy: &MetadataOnlyPolicy) -> Result<()> {
        ensure!(
            self.phase == Phase::MetadataSent,
            "metadata-only wait outside metadata state"
        );
        self.phase = Phase::MetadataOnlyWaiting;
        let mut manifest_seen = false;
        loop {
            let value = self.channel.receive().await?;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session metadata-only message"
            );
            match string(root, "Command")? {
                "SyncFinished" => {
                    if root.contains_key("Params") {
                        child(root, "Params")?;
                    }
                    self.phase = Phase::Finished;
                    return Ok(());
                }
                "AssetManifest" => {
                    ensure!(
                        !manifest_seen,
                        "repeated metadata-only manifest; no further sends"
                    );
                    // Full validation precedes the first refusal. No partial
                    // allowlist match can cause a reply to a malformed manifest.
                    let ancillary = check_metadata_manifest(child(root, "Params")?, policy)?;
                    manifest_seen = true;
                    self.decline_ancillary(ancillary).await?;
                }
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?
                }
                _ => bail!("unexpected metadata-only terminal/request; no further sends"),
            }
        }
    }

    /// Snapshot-bound metadata-only variant. A full matching-session manifest
    /// may contain only eligible foreign artwork/Genius requests. It never
    /// sends FileComplete, accepts own assets, or continues after invalid input.
    pub async fn wait_metadata_only_with_policy(
        &mut self,
        policy: &super::ancillary::MetadataPolicy<'_>,
    ) -> Result<()> {
        self.wait_refusing_own_asset(policy).await
    }

    /// Metadata-only wait for removing one represented row that has no file
    /// location. Same single-consumption rules as the installed-track wait:
    /// any request for the removed row's own asset ends the session.
    pub async fn wait_pending_delete(
        &mut self,
        policy: &super::ancillary::PendingDeletePolicy<'_>,
    ) -> Result<()> {
        self.wait_refusing_own_asset(policy.refusal()).await
    }

    async fn wait_refusing_own_asset(
        &mut self,
        policy: &super::ancillary::MetadataPolicy<'_>,
    ) -> Result<()> {
        ensure!(
            self.phase == Phase::MetadataSent,
            "metadata-only wait outside metadata state"
        );
        self.phase = Phase::MetadataOnlyWaiting;
        let host = child(
            child(dictionary(self.prepared.request())?, "Params")?,
            "HostInfo",
        )?;
        policy.ensure_session(
            string(host, "LibraryID")?,
            self.prepared.media_anchor.parse()?,
        )?;
        let mut manifest_seen = false;
        loop {
            let value = self.channel.receive().await?;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session metadata-only message"
            );
            match string(root, "Command")? {
                "SyncFinished" => {
                    if root.contains_key("Params") {
                        child(root, "Params")?;
                    }
                    self.phase = Phase::Finished;
                    return Ok(());
                }
                "AssetManifest" => {
                    ensure!(
                        !manifest_seen,
                        "repeated metadata-only manifest; no further sends"
                    );
                    let ancillary = policy.validate_manifest(&value, 2)?;
                    manifest_seen = true;
                    self.decline_ancillary(ancillary).await?;
                }
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?
                }
                _ => bail!("unexpected metadata-only terminal/request; no further sends"),
            }
        }
    }

    async fn wait_for_asset(
        &mut self,
        expected_asset_id: &str,
        part: AssetPart,
        allowed_ancillary: Option<&[(String, u64); 2]>,
    ) -> Result<()> {
        ensure!(
            self.phase == Phase::MetadataSent,
            "manifest wait outside metadata state"
        );
        loop {
            let value = self.channel.receive().await?;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session post-metadata message"
            );
            match string(root, "Command")? {
                "AssetManifest" => {
                    // Validate every record before retaining a request or sending any reply.
                    self.ancillary = check_asset_manifest(
                        child(root, "Params")?,
                        expected_asset_id,
                        part,
                        allowed_ancillary,
                    )?;
                    self.assets = vec![expected_asset_id.to_owned()];
                    self.phase = Phase::AssetRequested(part);
                    return Ok(());
                }
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?
                }
                _ => {
                    bail!("unexpected terminal/request before own asset manifest; no further sends")
                }
            }
        }
    }

    pub async fn complete_audio(&mut self, asset_id: &str, permanent_path: &str) -> Result<()> {
        validate_music_path(permanent_path)?;
        self.complete_asset(asset_id, permanent_path, AssetPart::Audio)
            .await
    }

    /// Report a completed MPEG transfer through the existing bounded session.
    /// This checks the wire phase and destination; the caller must already hold
    /// its durable operation authority and independently verify completion.
    pub async fn complete_mpeg_audio(
        &mut self,
        asset_id: &str,
        permanent_path: &str,
    ) -> Result<()> {
        validate_mpeg_music_path(permanent_path)?;
        self.complete_asset(asset_id, permanent_path, AssetPart::Audio)
            .await
    }

    /// Report a completed transfer of a file of `format`, whose destination
    /// carries that format's extension. The same duties as
    /// [`Self::complete_mpeg_audio`] stay with the caller.
    pub async fn complete_format_audio(
        &mut self,
        asset_id: &str,
        permanent_path: &str,
        format: crate::media::format::Format,
    ) -> Result<()> {
        validate_format_music_path(permanent_path, format)?;
        self.complete_asset(asset_id, permanent_path, AssetPart::Audio)
            .await
    }

    /// Native art-only completion has an empty AssetPath, no Variant, and no
    /// audio rename. Caller must first verify its staged raw image readback.
    pub async fn complete_artwork(&mut self, asset_id: &str) -> Result<()> {
        self.complete_asset(asset_id, "", AssetPart::Artwork).await
    }

    async fn complete_asset(
        &mut self,
        asset_id: &str,
        permanent_path: &str,
        part: AssetPart,
    ) -> Result<()> {
        let position = self.assets.iter().position(|own| own == asset_id);
        ensure!(
            self.phase == Phase::AssetRequested(part) && position.is_some(),
            "completion does not match accepted asset request"
        );
        let mut params = Dictionary::new();
        params.insert("AssetID".into(), asset_id.into());
        params.insert("AssetPath".into(), permanent_path.into());
        params.insert("Dataclass".into(), "Media".into());
        let mut value = Dictionary::new();
        value.insert("Session".into(), 2u64.into());
        value.insert("Command".into(), "FileComplete".into());
        value.insert("Params".into(), Value::Dictionary(params));
        self.send(&Value::Dictionary(value)).await?;
        self.assets
            .remove(position.context("accepted asset vanished")?);
        if !self.assets.is_empty() {
            return Ok(());
        }
        self.phase = Phase::CompleteSent;
        // Native completions are not ordered by manifest position. Every own
        // asset is uploaded/read back and FileComplete sent before refusing
        // reviewed unavailable ancillary assets. Never synthesize a failure
        // for an own asset.
        let ancillary = std::mem::take(&mut self.ancillary);
        self.decline_ancillary(ancillary).await
    }

    async fn decline_ancillary(&mut self, ancillary: Vec<String>) -> Result<()> {
        for ancillary in ancillary {
            let mut params = Dictionary::new();
            params.insert("AssetID".into(), ancillary.into());
            params.insert("Dataclass".into(), "Media".into());
            params.insert("ErrorCode".into(), Value::Integer(16i64.into()));
            params.insert("ErrorIsVisible".into(), false.into());
            let mut message = Dictionary::new();
            message.insert("Session".into(), 2u64.into());
            message.insert("Command".into(), "FileError".into());
            message.insert("Params".into(), Value::Dictionary(params));
            self.send(&Value::Dictionary(message)).await?;
        }
        Ok(())
    }

    pub async fn wait_finished(&mut self) -> Result<()> {
        ensure!(
            self.phase == Phase::CompleteSent,
            "terminal wait before own FileComplete"
        );
        loop {
            let value = self.channel.receive().await?;
            let root = dictionary(&value)?;
            ensure!(
                number(root, "Session")? == 2,
                "wrong-session terminal/progress"
            );
            match string(root, "Command")? {
                "SyncFinished" => {
                    if root.contains_key("Params") {
                        child(root, "Params")?;
                    }
                    self.phase = Phase::Finished;
                    return Ok(());
                }
                "Progress" | "SyncAllowed" | "InstalledAssets" | "AssetMetrics" => {
                    check_telemetry(root)?
                }
                _ => bail!("unexpected terminal/request after FileComplete; no further sends"),
            }
        }
    }
}

/// Validate the bounded telemetry shape used by live and retained sessions.
pub fn check_telemetry(root: &Dictionary) -> Result<()> {
    let params = child(root, "Params")?;
    match string(root, "Command")? {
        "SyncAllowed" => check_allowed(params),
        "Progress" => {
            exact_keys(params, &["OverallProgress"])?;
            let progress = params["OverallProgress"]
                .as_real()
                .context("non-real progress")?;
            ensure!(
                progress.is_finite() && (0.0..=1.0).contains(&progress),
                "invalid progress"
            );
            Ok(())
        }
        "InstalledAssets" | "AssetMetrics" => Ok(()),
        _ => bail!("unsupported telemetry"),
    }
}

fn check_asset_manifest(
    params: &Dictionary,
    expected_id: &str,
    expected_part: AssetPart,
    allowed_ancillary: Option<&[(String, u64); 2]>,
) -> Result<Vec<String>> {
    let assets = manifest_assets(params)?;
    ensure!(
        !assets.is_empty() && assets.len() <= if allowed_ancillary.is_some() { 3 } else { 1 },
        "manifest must request own asset and only reviewed ancillary assets"
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut own = false;
    let mut ancillary = Vec::new();
    for asset in assets {
        let (id, part) = manifest_request(asset)?;
        ensure!(seen.insert(id), "duplicate manifest identity");
        if id == expected_id {
            ensure!(
                part == expected_part as u64,
                "own asset must request exactly the expected single part"
            );
            own = true;
        } else {
            ensure!(
                [2, 4].contains(&part)
                    && allowed_ancillary.is_some_and(|allowed| allowed
                        .iter()
                        .any(|(known_id, known_part)| known_id == id && *known_part == part)),
                "unreviewed ancillary identity or part"
            );
            ancillary.push(id.to_owned());
        }
    }
    ensure!(own, "manifest omitted own asset");
    Ok(ancillary)
}

fn manifest_assets(params: &Dictionary) -> Result<&[Value]> {
    exact_keys(params, &["AssetManifest"])?;
    let manifest = child(params, "AssetManifest")?;
    exact_keys(manifest, &["Media"])?;
    Ok(manifest["Media"]
        .as_array()
        .context("Media manifest is not an array")?)
}

fn manifest_request(asset: &Value) -> Result<(&str, u64)> {
    let asset = dictionary(asset)?;
    exact_keys(asset, &["AssetID", "AssetType", "IsDownload", "Variant"])?;
    ensure!(
        string(asset, "AssetType")? == "Music" && asset["IsDownload"].as_boolean() == Some(true),
        "unsupported manifest type/direction"
    );
    let variant = child(asset, "Variant")?;
    exact_keys(variant, &["AssetParts"])?;
    Ok((string(asset, "AssetID")?, number(variant, "AssetParts")?))
}

fn check_metadata_manifest(
    params: &Dictionary,
    policy: &MetadataOnlyPolicy,
) -> Result<Vec<String>> {
    let assets = manifest_assets(params)?;
    ensure!(
        !assets.is_empty() && assets.len() <= 2,
        "metadata-only manifest must contain only reviewed ancillary requests"
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut accepted = Vec::new();
    for asset in assets {
        let (id, part) = manifest_request(asset)?;
        ensure!(
            id != policy.target
                && seen.insert(id)
                && policy
                    .ancillary
                    .iter()
                    .any(|(known, expected_part)| known == id && *expected_part == part),
            "own, duplicate, or unreviewed metadata-only asset request"
        );
        accepted.push(id.to_owned());
    }
    Ok(accepted)
}

pub fn validate_music_path(path: &str) -> Result<()> {
    validate_audio_path(path, ".m4a")
}

/// Accept only the qualified MPEG destination shape; this grants no write authority.
pub fn validate_mpeg_music_path(path: &str) -> Result<()> {
    validate_audio_path(path, ".mp3")
}

/// The destination shape of a file of `format`: four uppercase letters and
/// the format's extension.
pub fn validate_format_music_path(path: &str, format: crate::media::format::Format) -> Result<()> {
    validate_audio_path(path, format.extension())
}

fn validate_audio_path(path: &str, suffix: &str) -> Result<()> {
    let rest = path
        .strip_prefix("iTunes_Control/Music/")
        .context("invalid permanent Music root")?;
    let (folder, name) = rest.split_once('/').context("missing Music folder")?;
    ensure!(
        folder.len() == 3
            && folder.starts_with('F')
            && folder[1..].bytes().all(|b| b.is_ascii_digit()),
        "unsupported Music folder name"
    );
    ensure!(
        name.len() == 4 + suffix.len()
            && name.ends_with(suffix)
            && name[..4].bytes().all(|b| b.is_ascii_uppercase()),
        "unsupported permanent filename"
    );
    Ok(())
}

pub fn prepare(
    mut request: Value,
    finished: &Value,
    library_id: &str,
    saved_media_anchor: u32,
) -> Result<Prepared> {
    let root = dictionary(&request)?;
    exact_keys(root, &["Session", "Command", "Params"])?;
    ensure!(
        string(root, "Command")? == "RequestingSync",
        "wrong reference request command"
    );
    let old_session = number(root, "Session")?;
    ensure!(
        old_session > 0 && old_session <= u32::MAX.into(),
        "invalid captured session"
    );
    let params = child(root, "Params")?;
    exact_keys(params, &["HostInfo", "DataclassAnchors", "Dataclasses"])?;
    strings(params.get("Dataclasses"), &["Media", "Book", "Keybag"])?;
    let host = child(params, "HostInfo")?;
    exact_keys(
        host,
        &[
            "Grappa",
            "Version",
            "LibraryID",
            "SyncHostName",
            "SyncedDataclasses",
            "WindowsOSVersion",
            "Type",
            "Wakeable",
            "SyncedAssetTypes",
        ],
    )?;
    ensure!(
        string(host, "LibraryID")? == library_id,
        "captured and persisted library identities disagree"
    );
    for key in ["Version", "SyncHostName", "WindowsOSVersion"] {
        let value = string(host, key)?;
        ensure!(
            !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
            "invalid captured host text field"
        );
    }
    ensure!(
        string(host, "Type")? == "iTunes" && number(host, "Wakeable")? == 0,
        "unsupported captured host type/flags"
    );
    ensure!(
        host.get("Grappa")
            .and_then(Value::as_data)
            .is_some_and(|bytes| bytes.len() == 84),
        "missing/wrong length Grappa"
    );
    strings(host.get("SyncedDataclasses"), &["Media"])?;
    strings(host.get("SyncedAssetTypes"), &["Music"])?;
    let anchors = child(params, "DataclassAnchors")?;
    exact_keys(anchors, &["Media", "VoiceMemo"])?;
    let previous = anchor(anchors, "Media")?;
    anchor(anchors, "VoiceMemo")?;

    let end = dictionary(finished)?;
    exact_keys(end, &["Session", "Command", "Params"])?;
    ensure!(
        string(end, "Command")? == "FinishedSyncingMetadata"
            && number(end, "Session")? == old_session,
        "reference completion command/session mismatch"
    );
    let end_params = child(end, "Params")?;
    exact_keys(end_params, &["DataclassAnchors", "SyncTypes"])?;
    let sync_types = child(end_params, "SyncTypes")?;
    exact_keys(sync_types, &["Media", "Book", "Keybag"])?;
    ensure!(
        number(sync_types, "Media")? == 1
            && number(sync_types, "Book")? == 0
            && number(sync_types, "Keybag")? == 1,
        "reference is not the expected Media delta type 1"
    );
    let reference_anchors = child(end_params, "DataclassAnchors")?;
    exact_keys(reference_anchors, &["Media", "VoiceMemo"])?;
    ensure!(
        anchor(reference_anchors, "Media")? >= previous,
        "reference Media anchor moved backwards"
    );
    ensure!(
        string(anchors, "VoiceMemo")? == string(reference_anchors, "VoiceMemo")?,
        "unusual changed non-Media anchor"
    );
    ensure!(
        saved_media_anchor >= anchor(reference_anchors, "Media")?,
        "saved catalog Media anchor is older than the closed reference"
    );
    let media_anchor = saved_media_anchor.to_string();

    let root = request.as_dictionary_mut().unwrap();
    root.insert("Session".into(), 2u64.into());
    let params = root.get_mut("Params").unwrap().as_dictionary_mut().unwrap();
    params
        .get_mut("HostInfo")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("Grappa".into(), Value::Data(grappa::HOST_REQUEST.to_vec()));
    params
        .get_mut("DataclassAnchors")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("Media".into(), media_anchor.clone().into());
    Ok(Prepared {
        request,
        media_anchor,
        finished: finished.clone(),
        metadata_mode: MetadataMode::RevisionDelta,
    })
}

/// Validate the same exact reference/library/Delta1/VoiceMemo contract as an
/// ordinary preparation, then select the closed no-operation finish shape.
/// The current anchor is still checked against a genuine fresh Ready at connect.
/// There is no caller-supplied finished-anchor override or metadata payload.
pub fn prepare_empty_delta(
    request: Value,
    finished: &Value,
    library_id: &str,
    current_media_anchor: u32,
) -> Result<EmptyDeltaPrepared> {
    let mut prepared = prepare(request, finished, library_id, current_media_anchor)?;
    prepared.metadata_mode = MetadataMode::EmptyDelta;
    Ok(EmptyDeltaPrepared { prepared })
}

fn dictionary(value: &Value) -> Result<&Dictionary> {
    value.as_dictionary().context("expected dictionary")
}
fn child<'a>(parent: &'a Dictionary, key: &str) -> Result<&'a Dictionary> {
    dictionary(parent.get(key).with_context(|| format!("missing {key}"))?)
}
fn string<'a>(parent: &'a Dictionary, key: &str) -> Result<&'a str> {
    parent
        .get(key)
        .and_then(Value::as_string)
        .with_context(|| format!("missing/string type mismatch: {key}"))
}
fn number(parent: &Dictionary, key: &str) -> Result<u64> {
    parent
        .get(key)
        .and_then(Value::as_unsigned_integer)
        .with_context(|| format!("missing/unsigned integer type mismatch: {key}"))
}
fn exact_keys(value: &Dictionary, keys: &[&str]) -> Result<()> {
    ensure!(
        value.len() == keys.len() && keys.iter().all(|key| value.contains_key(key)),
        "unexpected dictionary fields"
    );
    Ok(())
}
fn strings(value: Option<&Value>, expected: &[&str]) -> Result<()> {
    let values = value
        .and_then(Value::as_array)
        .context("expected string array")?;
    ensure!(
        values.len() == expected.len()
            && values
                .iter()
                .zip(expected)
                .all(|(v, e)| v.as_string() == Some(*e)),
        "unexpected dataclass/asset-type array"
    );
    Ok(())
}
fn anchor(parent: &Dictionary, key: &str) -> Result<u32> {
    let text = string(parent, key)?;
    let parsed = text.parse::<u32>().context("invalid anchor string")?;
    ensure!(parsed.to_string() == text, "noncanonical anchor string");
    Ok(parsed)
}

fn check_allowed(params: &Dictionary) -> Result<()> {
    exact_keys(
        params,
        &[
            "ManualSync",
            "EducationModeEnabled",
            "AppRemovalAllowed",
            "AutoSync",
            "PurgeAllowed",
            "DataProtected",
            "AppInstallationAllowed",
            "DisabledAssetTypes",
            "SupportsFavorites",
        ],
    )?;
    for key in [
        "ManualSync",
        "EducationModeEnabled",
        "AppRemovalAllowed",
        "AutoSync",
        "PurgeAllowed",
        "DataProtected",
        "AppInstallationAllowed",
        "SupportsFavorites",
    ] {
        ensure!(
            params.get(key).and_then(Value::as_boolean).is_some(),
            "nonboolean readiness field"
        );
    }
    ensure!(
        params["ManualSync"].as_boolean() == Some(true)
            && params["DataProtected"].as_boolean() == Some(false)
            && params["EducationModeEnabled"].as_boolean() == Some(false),
        "device is not ready for this unprotected manual Media session"
    );
    strings(
        params.get("DisabledAssetTypes"),
        &["Photo", "Movie", "TVEpisode", "HomeVideo"],
    )
}

struct SessionTransport(atc::Atc);

impl IdeviceService for SessionTransport {
    fn service_name() -> Cow<'static, str> {
        atc::Atc::service_name()
    }
    async fn from_stream(device: Idevice) -> std::result::Result<Self, IdeviceError> {
        Ok(Self(budgeted(device, MAX_BYTES).await?))
    }
}

async fn budgeted(device: Idevice, limit: usize) -> std::result::Result<atc::Atc, IdeviceError> {
    let inner = device
        .get_socket()
        .ok_or(IdeviceError::NoEstablishedConnection)?;
    atc::Atc::from_stream(Idevice::new(
        Box::new(ByteBudget {
            inner,
            remaining: limit,
        }),
        "bounded-media",
    ))
    .await
}

/// Transport byte counter only: the existing Atc owns all framing. Both
/// directions share one cap, so an oversized aggregate is stopped mid-frame
/// and preserved by Atc's normal partial-frame path, without unsafe code.
#[derive(Debug)]
struct ByteBudget {
    inner: Box<dyn ReadWrite>,
    remaining: usize,
}

impl AsyncRead for ByteBudget {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.remaining == 0 {
            return Poll::Ready(Err(io::Error::other("ATC aggregate byte limit reached")));
        }
        let mut scratch = [0u8; 8192];
        let length = this.remaining.min(output.remaining()).min(scratch.len());
        let mut bounded = ReadBuf::new(&mut scratch[..length]);
        ready!(Pin::new(&mut this.inner).poll_read(cx, &mut bounded))?;
        this.remaining -= bounded.filled().len();
        output.put_slice(bounded.filled());
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for ByteBudget {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.remaining == 0 {
            return Poll::Ready(Err(io::Error::other("ATC aggregate byte limit reached")));
        }
        let count = ready!(
            Pin::new(&mut this.inner).poll_write(cx, &bytes[..bytes.len().min(this.remaining)])
        )?;
        this.remaining -= count;
        Poll::Ready(Ok(count))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    const ID: &str = "0000000000000003";
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "reference-session-test-{}-{}",
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
    fn dict(values: &[(&str, Value)]) -> Value {
        Value::Dictionary(
            values
                .iter()
                .map(|(k, v)| (String::from(*k), v.clone()))
                .collect(),
        )
    }
    fn array(values: &[&str]) -> Value {
        Value::Array(values.iter().map(|v| Value::String((*v).into())).collect())
    }
    fn set(value: &mut Value, path: &[&str], replacement: Value) {
        let mut current = value;
        for key in &path[..path.len() - 1] {
            current = current.as_dictionary_mut().unwrap().get_mut(key).unwrap();
        }
        current
            .as_dictionary_mut()
            .unwrap()
            .insert(path[path.len() - 1].into(), replacement);
    }
    fn references() -> (Value, Value) {
        let host = dict(&[
            ("Grappa", Value::Data(vec![1; 84])),
            ("Version", "13.2.4.123".into()),
            ("LibraryID", ID.into()),
            ("SyncHostName", "synthetic-test-host".into()),
            ("SyncedDataclasses", array(&["Media"])),
            ("WindowsOSVersion", "10.0".into()),
            ("Type", "iTunes".into()),
            ("Wakeable", 0u64.into()),
            ("SyncedAssetTypes", array(&["Music"])),
        ]);
        let request = dict(&[
            ("Session", 9u64.into()),
            ("Command", "RequestingSync".into()),
            (
                "Params",
                dict(&[
                    ("HostInfo", host),
                    (
                        "DataclassAnchors",
                        dict(&[("Media", "10".into()), ("VoiceMemo", "1".into())]),
                    ),
                    ("Dataclasses", array(&["Media", "Book", "Keybag"])),
                ]),
            ),
        ]);
        let finished = dict(&[
            ("Session", 9u64.into()),
            ("Command", "FinishedSyncingMetadata".into()),
            (
                "Params",
                dict(&[
                    (
                        "DataclassAnchors",
                        dict(&[("Media", "11".into()), ("VoiceMemo", "1".into())]),
                    ),
                    (
                        "SyncTypes",
                        dict(&[
                            ("Media", 1u64.into()),
                            ("Book", 0u64.into()),
                            ("Keybag", 1u64.into()),
                        ]),
                    ),
                ]),
            ),
        ]);
        (request, finished)
    }
    fn prepared() -> Prepared {
        let (request, finished) = references();
        prepare(request, &finished, ID, 11).unwrap()
    }
    fn wire(value: &Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.to_writer_binary(&mut bytes).unwrap();
        [(bytes.len() as u32).to_le_bytes().as_slice(), &bytes].concat()
    }
    async fn outgoing(peer: &mut DuplexStream) -> (Value, Vec<u8>) {
        let mut length = [0; 4];
        peer.read_exact(&mut length).await.unwrap();
        let size = u32::from_le_bytes(length) as usize;
        assert!(size <= MAX_BYTES);
        let mut payload = vec![0; size];
        peer.read_exact(&mut payload).await.unwrap();
        (
            plist::from_bytes(&payload).unwrap(),
            [length.as_slice(), &payload].concat(),
        )
    }
    fn incoming(command: &str, params: Option<Value>) -> Value {
        let mut value = dict(&[
            ("Command", command.into()),
            ("Session", 2u64.into()),
            ("Type", 0u64.into()),
            ("Id", 10u64.into()),
        ]);
        if let Some(params) = params {
            set(&mut value, &["Params"], params);
        }
        value
    }
    fn capabilities() -> Value {
        incoming(
            "Capabilities",
            Some(dict(&[(
                "GrappaSupportInfo",
                dict(&[
                    ("version", 1u64.into()),
                    ("deviceType", 0u64.into()),
                    ("protocolVersion", 1u64.into()),
                ]),
            )])),
        )
    }
    fn allowed() -> Value {
        incoming(
            "SyncAllowed",
            Some(dict(&[
                ("ManualSync", true.into()),
                ("EducationModeEnabled", false.into()),
                ("AppRemovalAllowed", true.into()),
                ("AutoSync", true.into()),
                ("PurgeAllowed", true.into()),
                ("DataProtected", false.into()),
                ("AppInstallationAllowed", true.into()),
                ("SupportsFavorites", true.into()),
                (
                    "DisabledAssetTypes",
                    array(&["Photo", "Movie", "TVEpisode", "HomeVideo"]),
                ),
            ])),
        )
    }
    fn ready_message() -> Value {
        let mut grappa = vec![7; 83];
        grappa[..2].copy_from_slice(&[1, 1]);
        grappa[18] = 0x40;
        incoming(
            "ReadyForSync",
            Some(dict(&[
                (
                    "DataclassAnchors",
                    dict(&[("Media", "11".into()), ("Book", "0".into())]),
                ),
                ("DeviceInfo", dict(&[("Grappa", Value::Data(grappa))])),
            ])),
        )
    }
    async fn startup(peer: &mut DuplexStream) {
        // Arrival order is deliberately different; Pong's Session is not an echo.
        let mut pong = incoming("Pong", None);
        set(&mut pong, &["Session"], 77u64.into());
        for value in [allowed(), capabilities(), pong] {
            peer.write_all(&wire(&value)).await.unwrap();
        }
    }
    async fn transport(limit: usize) -> (atc::Atc, DuplexStream) {
        let (client, peer) = tokio::io::duplex(64 * 1024);
        (
            budgeted(Idevice::new(Box::new(client), "offline-only"), limit)
                .await
                .unwrap(),
            peer,
        )
    }

    fn manifest(id: &str, parts: u64) -> Value {
        dict(&[(
            "AssetManifest",
            dict(&[(
                "Media",
                Value::Array(vec![dict(&[
                    ("AssetID", id.into()),
                    ("AssetType", "Music".into()),
                    ("IsDownload", true.into()),
                    ("Variant", dict(&[("AssetParts", parts.into())])),
                ])]),
            )]),
        )])
    }

    fn combined_manifest(records: &[(&str, u64)]) -> Value {
        let assets = records
            .iter()
            .map(|(id, part)| {
                let value = manifest(id, *part);
                child(value.as_dictionary().unwrap(), "AssetManifest").unwrap()["Media"]
                    .as_array()
                    .unwrap()[0]
                    .clone()
            })
            .collect();
        dict(&[("AssetManifest", dict(&[("Media", Value::Array(assets))]))])
    }

    #[test]
    fn ancillary_policy_rejects_unreviewed_mixed_duplicate_and_missing_audio() {
        let allowed = [("-888".to_owned(), 2), ("999".to_owned(), 4)];
        for records in [
            vec![("-888", 2), ("-12345", 1), ("new", 4)],
            vec![("-888", 3), ("-12345", 1)],
            vec![("-888", 4), ("-12345", 1)],
            vec![("-888", 2), ("-12345", 3)],
            vec![("-888", 2), ("-12345", 1), ("-888", 2)],
            vec![("-888", 2), ("999", 4)],
        ] {
            assert!(
                check_asset_manifest(
                    combined_manifest(&records).as_dictionary().unwrap(),
                    "-12345",
                    AssetPart::Audio,
                    Some(&allowed)
                )
                .is_err()
            );
        }
        let value = combined_manifest(&[("-888", 2), ("-12345", 1), ("999", 4)]);
        assert_eq!(
            check_asset_manifest(
                value.as_dictionary().unwrap(),
                "-12345",
                AssetPart::Audio,
                Some(&allowed)
            )
            .unwrap(),
            vec!["-888", "999"]
        );
        assert!(
            check_asset_manifest(
                value.as_dictionary().unwrap(),
                "-12345",
                AssetPart::Audio,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn exact_audio_manifest_and_permanent_path_guards() {
        for (id, parts) in [("-12345", 2), ("-12345", 3), ("12345", 1), ("other", 1)] {
            assert!(
                check_asset_manifest(
                    manifest(id, parts).as_dictionary().unwrap(),
                    "-12345",
                    AssetPart::Audio,
                    None
                )
                .is_err()
            );
        }
        check_asset_manifest(
            manifest("-12345", 1).as_dictionary().unwrap(),
            "-12345",
            AssetPart::Audio,
            None,
        )
        .unwrap();
        for path in [
            "/iTunes_Control/Music/F00/ABCD.m4a",
            "iTunes_Control/Music/F0/ABCD.m4a",
            "iTunes_Control/Music/F00/abcd.m4a",
            "iTunes_Control/Music/F00/ABCD 1.m4a",
            "Airlock/Media/123.m4a",
        ] {
            assert!(validate_music_path(path).is_err());
        }
        validate_music_path("iTunes_Control/Music/F09/ABCD.m4a").unwrap();
        let ready = prepared();
        assert_eq!(ready.next_anchor().unwrap(), 12);
        let end = ready.finished_message().unwrap();
        assert_eq!(
            end.as_dictionary().unwrap()["Session"].as_unsigned_integer(),
            Some(2)
        );
    }

    #[test]
    fn artwork_manifest_requires_owned_single_part_and_reviewed_ancillary() {
        let allowed = [("-888".to_owned(), 2), ("999".to_owned(), 4)];
        for records in [
            vec![("-12345", 1)],
            vec![("-12345", 3)],
            vec![("12345", 2)],
            vec![("-12345", 2), ("unexpected", 2)],
            vec![("-12345", 2), ("-12345", 2)],
            vec![("-888", 2), ("999", 4)],
        ] {
            assert!(
                check_asset_manifest(
                    combined_manifest(&records).as_dictionary().unwrap(),
                    "-12345",
                    AssetPart::Artwork,
                    Some(&allowed),
                )
                .is_err()
            );
        }
        assert_eq!(
            check_asset_manifest(
                combined_manifest(&[("-888", 2), ("-12345", 2), ("999", 4)])
                    .as_dictionary()
                    .unwrap(),
                "-12345",
                AssetPart::Artwork,
                Some(&allowed),
            )
            .unwrap(),
            vec!["-888", "999"]
        );
    }

    #[test]
    fn mpeg_path_does_not_expand_the_ordinary_alac_path() {
        validate_mpeg_music_path("iTunes_Control/Music/F40/ABCD.mp3").unwrap();
        assert!(validate_music_path("iTunes_Control/Music/F40/ABCD.mp3").is_err());
        assert!(validate_mpeg_music_path("iTunes_Control/Music/F40/ABCD.m4a").is_err());
        for p in [
            "/iTunes_Control/Music/F40/ABCD.mp3",
            "iTunes_Control/Music/F4/ABCD.mp3",
            "iTunes_Control/Music/F40/abcd.mp3",
            "iTunes_Control/Music/F40/ABCD.MP3",
            "iTunes_Control/Music/F40/../ABCD.mp3",
        ] {
            assert!(validate_mpeg_music_path(p).is_err());
        }
    }

    #[tokio::test]
    async fn complete_session_has_exact_asset_path_and_owned_completion_before_ancillary() {
        for (part, mpeg) in [
            (AssetPart::Audio, false),
            (AssetPart::Artwork, false),
            (AssetPart::Audio, true),
        ] {
            for with_ancillary in [false, true] {
                let trace = Temporary::new();
                let (client, mut peer) = transport(MAX_BYTES).await;
                let host = async {
                    let now = Instant::now();
                    let mut session = Session::begin(
                        client,
                        prepared(),
                        &trace.0,
                        now + Duration::from_secs(2),
                        now + Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                    assert_eq!(session.sign_metadata(b"exact operation").unwrap().len(), 21);
                    assert_eq!(
                        session.ready_receipt_sha256(),
                        <[u8; 32]>::from(Sha256::digest(wire(&ready_message())))
                    );
                    assert!(session.terminal_receipt_sha256().is_err());
                    session.finish_metadata().await.unwrap();
                    assert!(session.finish_metadata().await.is_err());
                    let allowed = [("-888".to_owned(), 2), ("999".to_owned(), 4)];
                    if part == AssetPart::Audio {
                        session
                            .wait_for_audio("-12345", with_ancillary.then_some(&allowed))
                            .await
                            .unwrap();
                        assert!(session.complete_artwork("-12345").await.is_err());
                        if mpeg {
                            assert!(
                                session
                                    .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.mp3")
                                    .await
                                    .is_err()
                            );
                            assert!(
                                session
                                    .complete_mpeg_audio("999", "iTunes_Control/Music/F00/ABCD.mp3")
                                    .await
                                    .is_err()
                            );
                            session
                                .complete_mpeg_audio("-12345", "iTunes_Control/Music/F00/ABCD.mp3")
                                .await
                                .unwrap();
                            assert!(
                                session
                                    .complete_mpeg_audio(
                                        "-12345",
                                        "iTunes_Control/Music/F00/ABCD.mp3"
                                    )
                                    .await
                                    .is_err()
                            );
                        } else {
                            session
                                .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.m4a")
                                .await
                                .unwrap();
                        }
                    } else {
                        session
                            .wait_for_artwork("-12345", with_ancillary.then_some(&allowed))
                            .await
                            .unwrap();
                        assert!(
                            session
                                .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.m4a")
                                .await
                                .is_err()
                        );
                        assert!(session.complete_artwork("12345").await.is_err());
                        session.complete_artwork("-12345").await.unwrap();
                    }
                    session.wait_finished().await.unwrap();
                    assert_eq!(
                        session.terminal_receipt_sha256().unwrap(),
                        <[u8; 32]>::from(Sha256::digest(wire(&incoming("SyncFinished", None))))
                    );
                };
                let device = async {
                    assert_eq!(
                        outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                        Some("Ping")
                    );
                    startup(&mut peer).await;
                    assert_eq!(
                        outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                        Some("RequestingSync")
                    );
                    peer.write_all(&wire(&ready_message())).await.unwrap();
                    let end = outgoing(&mut peer).await.0;
                    let end = end.as_dictionary().unwrap();
                    assert_eq!(end["Command"].as_string(), Some("FinishedSyncingMetadata"));
                    assert_eq!(
                        child(child(end, "Params").unwrap(), "DataclassAnchors").unwrap()["Media"]
                            .as_string(),
                        Some("12")
                    );
                    peer.write_all(&wire(&incoming(
                        "AssetManifest",
                        Some(if with_ancillary {
                            combined_manifest(&[("-888", 2), ("-12345", part as u64), ("999", 4)])
                        } else {
                            manifest("-12345", part as u64)
                        }),
                    )))
                    .await
                    .unwrap();
                    let complete = outgoing(&mut peer).await.0;
                    assert_eq!(
                        complete,
                        dict(&[
                            ("Session", 2u64.into()),
                            ("Command", "FileComplete".into()),
                            (
                                "Params",
                                dict(&[
                                    ("AssetID", "-12345".into()),
                                    (
                                        "AssetPath",
                                        if part == AssetPart::Artwork {
                                            ""
                                        } else if mpeg {
                                            "iTunes_Control/Music/F00/ABCD.mp3"
                                        } else {
                                            "iTunes_Control/Music/F00/ABCD.m4a"
                                        }
                                        .into()
                                    ),
                                    ("Dataclass", "Media".into()),
                                ])
                            ),
                        ])
                    );
                    if with_ancillary {
                        for id in ["-888", "999"] {
                            let value = outgoing(&mut peer).await.0;
                            let root = value.as_dictionary().unwrap();
                            exact_keys(root, &["Command", "Session", "Params"]).unwrap();
                            assert_eq!(root["Command"].as_string(), Some("FileError"));
                            assert_eq!(root["Session"].as_unsigned_integer(), Some(2));
                            let params = child(root, "Params").unwrap();
                            exact_keys(
                                params,
                                &["AssetID", "Dataclass", "ErrorCode", "ErrorIsVisible"],
                            )
                            .unwrap();
                            assert_eq!(params["AssetID"].as_string(), Some(id));
                            assert_eq!(params["Dataclass"].as_string(), Some("Media"));
                            assert_eq!(params["ErrorCode"].as_signed_integer(), Some(16));
                            assert_eq!(params["ErrorIsVisible"].as_boolean(), Some(false));
                        }
                    }
                    peer.write_all(&wire(&incoming("SyncFinished", None)))
                        .await
                        .unwrap();
                    let mut byte = [0];
                    assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
                };
                tokio::join!(host, device);
            }
        }
    }

    #[tokio::test]
    async fn a_phone_ping_is_answered_in_its_session_and_reading_goes_on() {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let mut channel = Channel {
            client,
            trace: trace.0.clone(),
            deadline: Instant::now() + Duration::from_secs(2),
            frames: 0,
            last_received_sha256: [0; 32],
        };
        let progress = incoming("Progress", Some(dict(&[("OverallProgress", 0.5.into())])));
        let device = async {
            peer.write_all(&wire(&incoming("Ping", None)))
                .await
                .unwrap();
            assert_eq!(
                outgoing(&mut peer).await.0,
                dict(&[("Session", 2u64.into()), ("Command", "Pong".into())])
            );
            peer.write_all(&wire(&progress)).await.unwrap();
            // A Ping with parameters is not one.
            peer.write_all(&wire(&incoming("Ping", Some(dict(&[])))))
                .await
                .unwrap();
        };
        let host = async {
            assert_eq!(channel.receive().await.unwrap(), progress);
            assert!(channel.receive().await.is_err());
        };
        tokio::join!(device, host);
        // Ping, Pong, Progress and the refused Ping are all traced.
        assert_eq!(channel.frames, 4);
        assert!(trace.0.join("001-outgoing.plist").is_file());
    }

    #[tokio::test]
    async fn expired_deadline_performs_no_ready_io_and_preserves_input() {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        peer.write_all(&wire(&incoming("Pong", None)))
            .await
            .unwrap();
        let mut channel = Channel {
            client,
            trace: trace.0.clone(),
            deadline: Instant::now() - Duration::from_millis(1),
            frames: 0,
            last_received_sha256: [0; 32],
        };
        assert!(channel.send(&incoming("Ping", None)).await.is_err());
        assert!(channel.receive().await.is_err());
        assert!(channel.client.partial_frame().is_empty());
        assert_eq!(channel.frames, 0);
        assert!(std::fs::read_dir(&trace.0).unwrap().next().is_none());
    }

    fn metadata_policy() -> MetadataOnlyPolicy {
        MetadataOnlyPolicy::new("-12345", [("-888".into(), 2), ("999".into(), 4)]).unwrap()
    }

    async fn metadata_transcript(messages: Vec<Value>, errors: &[&str], success: bool) {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let host = async {
            let now = Instant::now();
            let mut session = Session::begin(
                client,
                prepared(),
                &trace.0,
                now + Duration::from_secs(2),
                now + Duration::from_secs(1),
            )
            .await
            .unwrap();
            session.finish_metadata().await.unwrap();
            assert_eq!(
                session.wait_metadata_only(&metadata_policy()).await.is_ok(),
                success
            );
            // Both terminal and error consume this flow. A caller cannot turn
            // an unexpected own request into an upload or retry a late wait.
            assert!(
                session
                    .wait_metadata_only(&metadata_policy())
                    .await
                    .is_err()
            );
            assert!(session.wait_for_audio("-12345", None).await.is_err());
            assert!(session.complete_artwork("-12345").await.is_err());
            assert!(session.wait_finished().await.is_err());
        };
        let device = async {
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("Ping")
            );
            startup(&mut peer).await;
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("RequestingSync")
            );
            peer.write_all(&wire(&ready_message())).await.unwrap();
            let end = outgoing(&mut peer).await.0;
            assert_eq!(end, prepared().finished_message().unwrap());
            for message in messages {
                peer.write_all(&wire(&message)).await.unwrap();
            }
            for id in errors {
                assert_eq!(
                    outgoing(&mut peer).await.0,
                    dict(&[
                        ("Session", 2u64.into()),
                        ("Command", "FileError".into()),
                        (
                            "Params",
                            dict(&[
                                ("AssetID", (*id).into()),
                                ("Dataclass", "Media".into()),
                                ("ErrorCode", Value::Integer(16i64.into())),
                                ("ErrorIsVisible", false.into())
                            ])
                        ),
                    ])
                );
            }
            let mut remaining = Vec::new();
            peer.read_to_end(&mut remaining).await.unwrap();
            assert!(
                remaining.is_empty(),
                "unexpected outgoing reply or synthetic FileComplete"
            );
        };
        tokio::join!(host, device);
    }

    #[tokio::test]
    async fn metadata_only_accepts_terminal_without_any_asset_completion() {
        metadata_transcript(
            vec![
                incoming(
                    "Progress",
                    Some(dict(&[("OverallProgress", Value::Real(0.5))])),
                ),
                incoming("SyncFinished", None),
            ],
            &[],
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn metadata_only_declines_exact_ancillary_subset_then_accepts_terminal() {
        for records in [
            vec![("-888", 2)],
            vec![("999", 4)],
            vec![("999", 4), ("-888", 2)],
        ] {
            let errors: Vec<_> = records.iter().map(|(id, _)| *id).collect();
            metadata_transcript(
                vec![
                    incoming("AssetManifest", Some(combined_manifest(&records))),
                    incoming("SyncFinished", Some(dict(&[]))),
                ],
                &errors,
                true,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn metadata_only_validates_entire_manifest_before_any_reply() {
        for records in [
            vec![("-888", 2), ("-12345", 1)], // Own audio after allowed art.
            vec![("-888", 2), ("-12345", 2)], // Own art is also forbidden.
            vec![("-888", 2), ("123", 4)],    // Unknown after allowed first row.
            vec![("-888", 2), ("999", 5)],    // Mixed bits after allowed art.
            vec![("-888", 2), ("-888", 2)],
            vec![("-888", 4)], // Known ID with wrong part.
            vec![],
        ] {
            metadata_transcript(
                vec![incoming("AssetManifest", Some(combined_manifest(&records)))],
                &[],
                false,
            )
            .await;
        }
        for change in ["upload", "type", "extra"] {
            let mut manifest = combined_manifest(&[("-888", 2), ("999", 4)]);
            let row = manifest
                .as_dictionary_mut()
                .unwrap()
                .get_mut("AssetManifest")
                .unwrap()
                .as_dictionary_mut()
                .unwrap()
                .get_mut("Media")
                .unwrap()
                .as_array_mut()
                .unwrap()[1]
                .as_dictionary_mut()
                .unwrap();
            match change {
                "upload" => {
                    row.insert("IsDownload".into(), false.into());
                }
                "type" => {
                    row.insert("AssetType".into(), "Book".into());
                }
                _ => {
                    row.insert("Unexpected".into(), true.into());
                }
            }
            metadata_transcript(vec![incoming("AssetManifest", Some(manifest))], &[], false).await;
        }
    }

    #[tokio::test]
    async fn metadata_only_stops_wrong_session_and_late_or_repeated_requests() {
        for command in ["SyncFinished", "AssetManifest"] {
            let mut message = incoming(
                command,
                (command == "AssetManifest").then(|| combined_manifest(&[("-888", 2)])),
            );
            set(&mut message, &["Session"], 99u64.into());
            metadata_transcript(vec![message], &[], false).await;
        }
        let first = incoming("AssetManifest", Some(combined_manifest(&[("-888", 2)])));
        for late in [
            incoming("AssetManifest", Some(combined_manifest(&[("999", 4)]))),
            incoming("RequestingSync", Some(dict(&[]))),
            incoming("SyncFailed", Some(dict(&[]))),
        ] {
            metadata_transcript(vec![first.clone(), late], &["-888"], false).await;
        }
    }

    #[test]
    fn metadata_only_policy_excludes_own_and_noncanonical_or_duplicate_ids() {
        for allowed in [
            [("-12345".into(), 2), ("999".into(), 4)],
            [("-888".into(), 2), ("-888".into(), 4)],
            [("-888".into(), 2), ("999".into(), 2)],
            [("-888".into(), 1), ("999".into(), 4)],
            [("-0888".into(), 2), ("999".into(), 4)],
            [("-888".into(), 2), ("0".into(), 4)],
        ] {
            assert!(MetadataOnlyPolicy::new("-12345", allowed).is_err());
        }
        assert!(MetadataOnlyPolicy::new("own", [("-888".into(), 2), ("999".into(), 4)]).is_err());
    }

    mod bounded;
    mod dynamic;
    mod empty_delta;
    mod empty_delta_observe;
    mod pending_audio;
    mod stage;
}
