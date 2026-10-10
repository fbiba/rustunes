//! Insert batches. An insert batch is one signed metadata operation that inserts several
//! tracks, each with its cover or none; a cover reserves its own artwork cache
//! ID for the batch. It is journaled Prepared -> Started -> Verified (every
//! target bound as an ordinary active binding), or Cancelled, or NotApplied
//! after a fresh observation proves a sent batch did not apply, or Partial when
//! only some rows reached the catalogue with a file. A Partial batch is
//! outstanding until a verified removal batch takes its locationless rows out
//! again (Compensated).
//!
//! At most one native write of any kind is outstanding: an attempt, a removal
//! batch, an insert batch or a metadata batch. Rust
//! checks this and so do triggers, in every direction. The exceptions are a
//! removal batch that compensates the Started Insert or the Partial insert
//! batch, and the two halves of one session (`session`).
//!
//! A target the phone created and deleted again within the session has
//! outcome 4; a batch whose unbound targets were all deleted that way settles
//! Compensated directly.
use super::coordination::{self, CoordinationId, Work};
use super::*;
use std::collections::BTreeMap;

const OUTSTANDING_BATCH: &str = "an outstanding insert batch requires settlement";
const OUTSTANDING_STATES: &str = "0,1,5";

fn outstanding_id(c: &Connection) -> Result<Option<i64>> {
    Ok(c.query_row(
        &format!("SELECT id FROM insert_batches WHERE state IN({OUTSTANDING_STATES})"),
        [],
        |r| r.get(0),
    )
    .optional()?)
}

/// Idle checks for an outstanding insert or metadata batch.
pub(super) fn ensure_no_outstanding_batch(c: &Connection) -> Result<()> {
    ensure!(outstanding_id(c)?.is_none(), OUTSTANDING_BATCH);
    ensure!(
        metadata_batch::outstanding_id(c)?.is_none(),
        metadata_batch::OUTSTANDING_BATCH
    );
    Ok(())
}

/// The attempt history: bounded, and its AUTOINCREMENT sequence is exactly the
/// highest attempt ID (none before the first attempt).
fn validate_attempt_sequence(c: &Connection) -> Result<()> {
    ensure!(
        c.query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))? <= 100_000,
        "attempt history bound"
    );
    let maximum: i64 = c.query_row("SELECT COALESCE(MAX(id),0) FROM attempts", [], |r| r.get(0))?;
    let sequence: Option<i64> = c
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='attempts'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        sequence == (maximum != 0).then_some(maximum),
        "attempt sequence differs"
    );
    Ok(())
}

/// At most one native write is outstanding. A removal batch may follow the
/// Partial insert batch it compensates, or sit beside the insert half of its
/// own session.
pub(super) fn validate_exclusion(c: &Connection) -> Result<()> {
    let bad = |sql: &str| -> Result<bool> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM insert_batches i JOIN attempts a ON a.state IN(0,1) WHERE i.state IN(0,1,5))"
        )?,
        "an insert batch and an attempt are outstanding together"
    );
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM insert_batches i JOIN removal_batches b ON b.state IN(0,1) WHERE i.state IN(0,1,5) AND b.compensates_insert_batch IS NOT i.id AND b.with_insert_batch IS NOT i.id)"
        )?,
        "an insert batch and a removal batch are outstanding together"
    );
    // A session's halves move together: both Prepared, both Started, or the
    // removal half settled first.
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM removal_batches b JOIN insert_batches i ON i.id=b.with_insert_batch WHERE NOT ((b.state=0 AND i.state=0) OR (b.state=1 AND i.state=1) OR (b.state=2 AND i.state IN(2,5,6)) OR (b.state=3 AND i.state=3) OR (b.state=4 AND i.state=4)))"
        )?,
        "a session's removal and insert halves are in different states"
    );
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM insert_batches i JOIN metadata_batches m ON m.state IN(0,1) WHERE i.state IN(0,1,5))"
        )?,
        "an insert batch and a metadata batch are outstanding together"
    );
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM metadata_batches m JOIN attempts a ON a.state IN(0,1) WHERE m.state IN(0,1))"
        )?,
        "a metadata batch and an attempt are outstanding together"
    );
    ensure!(
        !bad(
            "SELECT EXISTS(SELECT 1 FROM metadata_batches m JOIN removal_batches b ON b.state IN(0,1) WHERE m.state IN(0,1))"
        )?,
        "a metadata batch and a removal batch are outstanding together"
    );
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct InsertTarget {
    pub content: Content,
    pub new_album_pid: Option<u64>,
    pub new_artist_pid: Option<u64>,
    /// The FLAC this target's ALAC was converted from (`sync --flac-to-alac`).
    pub conversion: Option<Conversion>,
    /// The absolute path of the file this target is sent from (a converted
    /// FLAC's own path). Its binding keeps it as the track's source, so
    /// later runs name it without reading this batch's run directory.
    pub source: Option<PathBuf>,
}
impl std::fmt::Debug for InsertTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsertTarget")
            .field("track", &self.content.track_pid)
            .field("path", &self.content.path)
            .field("new_album_pid", &self.new_album_pid)
            .field("new_artist_pid", &self.new_artist_pid)
            .field("converted", &self.conversion.is_some())
            .finish()
    }
}

/// The FLAC an inserted ALAC was converted from. Later plans match the FLAC to
/// its track through this link, never by converting it again, and its path is
/// the track's source path for `--remove-missing` and replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversion {
    /// The FLAC's absolute path when it was converted.
    pub source_path: PathBuf,
    pub source_bytes: u64,
    pub source_sha256: [u8; 32],
    /// The FLAC's encoded audio identity (`EncodedAudio`): what is matched.
    pub source_audio: [u8; 32],
    /// The first line of `ffmpeg -version` of the FFmpeg that converted it.
    pub ffmpeg: String,
}

impl Conversion {
    fn validate(&self) -> Result<()> {
        let path = self
            .source_path
            .to_str()
            .context("conversion source path is not UTF-8")?;
        ensure!(
            path.starts_with('/') && (2..=4096).contains(&path.len()),
            "conversion source path must be absolute"
        );
        ensure!(
            (1..=512 * 1024 * 1024).contains(&self.source_bytes),
            "conversion source size out of bounds"
        );
        nonzero_hash(&self.source_sha256)?;
        nonzero_hash(&self.source_audio)?;
        ensure!(
            (1..=256).contains(&self.ffmpeg.len()) && !self.ffmpeg.chars().any(char::is_control),
            "invalid FFmpeg version line"
        );
        Ok(())
    }
}
/// Fresh observations bound into the Prepared row. The operation is the exact
/// signed metadata operation, so its timestamp is fixed at preparation.
#[derive(Clone)]
pub struct InsertIntent {
    /// The Active coordination this batch is the next step of, if any.
    pub coordination: Option<CoordinationId>,
    pub operation: Vec<u8>,
    pub base_anchor: u32,
    pub before_current: [u8; 32],
    pub before_index: [u8; 32],
    pub input_receipt: [u8; 32],
    /// Absolute run directory holding the saved Before evidence.
    pub evidence: PathBuf,
    pub targets: Vec<InsertTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InsertBatchId(pub(super) i64);
impl InsertBatchId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertBatchState {
    Prepared,
    Started,
    /// Every target is bound.
    Verified,
    /// Never sent.
    Cancelled,
    /// Sent, then proven unapplied by a fresh observation.
    NotApplied,
    /// Some rows reached the catalogue without a file; outstanding until compensated.
    Partial,
    /// No unbound target is left in the catalogue: the locationless rows of a
    /// Partial batch were removed by a verified removal batch, or the phone
    /// deleted every unbound target itself.
    Compensated,
}
impl InsertBatchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Started => "started",
            Self::Verified => "verified",
            Self::Cancelled => "cancelled",
            Self::NotApplied => "not_applied",
            Self::Partial => "partial",
            Self::Compensated => "compensated",
        }
    }
    fn decode(n: i64) -> Result<Self> {
        Ok(match n {
            0 => Self::Prepared,
            1 => Self::Started,
            2 => Self::Verified,
            3 => Self::Cancelled,
            4 => Self::NotApplied,
            5 => Self::Partial,
            6 => Self::Compensated,
            _ => bail!("invalid insert batch state"),
        })
    }
}
/// What settlement found of one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOutcome {
    /// Verified and bound as an active binding.
    Bound,
    /// A catalogue row without its file, awaiting a compensating removal.
    PendingCompensation,
    /// Removed again by a verified compensating removal batch.
    Compensated,
    /// Created and deleted again by the phone within the session.
    RemovedByPhone,
}
impl TargetOutcome {
    fn decode(n: Option<i64>) -> Result<Option<Self>> {
        Ok(match n {
            None => None,
            Some(1) => Some(Self::Bound),
            Some(2) => Some(Self::PendingCompensation),
            Some(3) => Some(Self::Compensated),
            Some(4) => Some(Self::RemovedByPhone),
            _ => bail!("invalid insert target outcome"),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertTargetRecord {
    pub target: InsertTarget,
    pub outcome: Option<TargetOutcome>,
    /// The binding version a bound target created.
    pub binding: Option<(BindingId, u32)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertBatchRecord {
    pub id: InsertBatchId,
    pub coordination: Option<CoordinationId>,
    pub state: InsertBatchState,
    pub operation: Vec<u8>,
    pub base_anchor: u32,
    pub before_current: [u8; 32],
    pub before_index: [u8; 32],
    pub input_receipt: [u8; 32],
    pub evidence: PathBuf,
    pub ready_receipt: Option<[u8; 32]>,
    pub signature: Option<[u8; 21]>,
    pub result_receipt: Option<[u8; 32]>,
    pub verified_anchor: Option<u32>,
    pub targets: Vec<InsertTargetRecord>,
}
/// Insert batches by state.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InsertBatchCounts {
    pub prepared: u64,
    pub started: u64,
    pub verified: u64,
    pub cancelled: u64,
    pub not_applied: u64,
    pub partial: u64,
    pub compensated: u64,
}

/// Returned only after the durable Started commit. Not Clone: one send. A lost
/// value requires fresh reconciliation, never another permit.
pub struct StartedInsertBatch {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedInsertBatch {
    pub fn operation(&self) -> &[u8] {
        &self.operation
    }
    pub fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}
impl music_protocol::protocol::session::StartedOperation for StartedInsertBatch {
    fn operation(&self) -> &[u8] {
        StartedInsertBatch::operation(self)
    }
    fn signature(&self) -> &[u8; 21] {
        StartedInsertBatch::signature(self)
    }
}

/// The tracks, paths, audio and collections of one batch must be distinct
/// from each other, and each target's content and conversion valid.
fn validate_intent(intent: &InsertIntent) -> Result<()> {
    ensure!(
        !intent.operation.is_empty() && intent.operation.len() <= MAX_OPERATION,
        "insert operation exceeds bounds"
    );
    ensure!(
        intent.base_anchor < u32::MAX,
        "insert base anchor exhausted"
    );
    for hash in [
        &intent.before_current,
        &intent.before_index,
        &intent.input_receipt,
    ] {
        nonzero_hash(hash)?;
    }
    coordination::evidence_text(&intent.evidence)?;
    ensure!(
        !intent.targets.is_empty() && intent.targets.len() <= coordination::MAX_TARGETS,
        "an insert batch inserts between one and {} tracks",
        coordination::MAX_TARGETS
    );
    let (mut tracks, mut paths, mut audio) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut albums, mut artists, mut caches) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut converted = BTreeSet::new();
    for target in &intent.targets {
        let content = &target.content;
        // ALAC or MP3 (`validate_content` checks the path's extension). A
        // cover rides in the same session under its own new cache ID.
        validate_content(content)?;
        if let Some(conversion) = &target.conversion {
            conversion.validate()?;
            ensure!(content.source.is_alac(), "only an ALAC target is converted");
            ensure!(
                converted.insert(conversion.source_audio),
                "insert batch converts the same audio twice"
            );
        }
        if let Some(artwork) = &content.artwork {
            ensure!(
                caches.insert(artwork.cache_id),
                "insert batch lists an artwork cache ID twice"
            );
        }
        ensure!(
            tracks.insert(content.track_pid),
            "insert batch lists a track twice"
        );
        ensure!(
            paths.insert(content.path.to_ascii_lowercase()),
            "insert batch lists a path twice"
        );
        ensure!(
            audio.insert(content.source.audio.sha256),
            "insert batch lists the same audio twice"
        );
        for (pid, set) in [
            (target.new_album_pid, &mut albums),
            (target.new_artist_pid, &mut artists),
        ] {
            if let Some(pid) = pid {
                ensure!(pid != 0, "new identities must be nonzero and distinct");
                set.insert(pid);
            }
        }
        ensure!(
            target.new_album_pid.is_none() || target.new_album_pid != target.new_artist_pid,
            "new identities must be nonzero and distinct"
        );
    }
    ensure!(
        albums.is_disjoint(&artists) && albums.is_disjoint(&tracks) && artists.is_disjoint(&tracks),
        "new identities must be nonzero and distinct"
    );
    Ok(())
}

/// The artwork cache IDs an insert batch reserves in namespace 2, one per cover.
fn reserved_caches(targets: &[InsertTarget]) -> impl Iterator<Item = u32> + '_ {
    targets
        .iter()
        .filter_map(|t| t.content.artwork.as_ref().map(|a| a.cache_id))
}

/// Every identity an insert batch reserves in namespace 1, each once: the
/// tracks, then the new albums, then the new artists. Collections shared by
/// several targets are reserved once.
fn reserved_pids(targets: &[InsertTarget]) -> Vec<u64> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for pid in targets
        .iter()
        .map(|t| Some(t.content.track_pid))
        .chain(targets.iter().map(|t| t.new_album_pid))
        .chain(targets.iter().map(|t| t.new_artist_pid))
        .flatten()
    {
        if seen.insert(pid) {
            out.push(pid);
        }
    }
    out
}

/// The reservations an insert batch owns, with whether each is consumed: every
/// track, path and cover cache ID, and every collection it allocates; bound
/// targets (and the collections they use) are consumed.
fn expected_reservations(record: &InsertBatchRecord) -> BTreeMap<(i64, Vec<u8>), bool> {
    let mut out = BTreeMap::new();
    for t in &record.targets {
        let bound = t.outcome == Some(TargetOutcome::Bound);
        let content = &t.target.content;
        out.insert((1, content.track_pid.to_be_bytes().to_vec()), bound);
        out.insert((3, content.path.to_ascii_lowercase().into_bytes()), bound);
        if let Some(artwork) = &content.artwork {
            out.insert((2, artwork.cache_id.to_be_bytes().to_vec()), bound);
        }
        for pid in [t.target.new_album_pid, t.target.new_artist_pid]
            .into_iter()
            .flatten()
        {
            *out.entry((1, pid.to_be_bytes().to_vec())).or_insert(false) |= bound;
        }
    }
    out
}

/// The first version of a bound target's binding is what its batch created.
fn validate_binding(
    c: &Connection,
    record: &InsertBatchRecord,
    content: &Content,
    binding: BindingId,
) -> Result<()> {
    type Row = (Vec<u8>, i64, Option<i64>, Vec<u8>, i64, Option<i64>, i64);
    let (audio, provenance, first, stored, anchor, attempt, kind): Row = c
        .query_row(
            "SELECT r.audio,r.provenance,r.first_attempt,v.content,v.anchor,v.attempt,v.receipt_kind FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id AND v.version=1 WHERE r.id=?1 AND r.track=?2 AND r.path=?3",
            params![binding.as_i64(), content.track_pid.to_be_bytes(), content.path],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?
        .context("bound insert target lost its binding")?;
    ensure!(
        audio == content.source.audio.sha256
            && provenance == 2
            && first.is_none()
            && attempt.is_none()
            && kind == 3
            && stored == encode_content(content)?
            && Some(u32::try_from(anchor)?) == record.verified_anchor,
        "a bound insert target differs from its binding"
    );
    Ok(())
}

/// The verified compensating removal batch took `removed` locationless rows
/// out: those targets become Compensated and the insert batch with them.
pub(super) fn settle_compensated(
    tx: &Connection,
    batch: InsertBatchId,
    removed: usize,
) -> Result<()> {
    ensure!(
        tx.execute(
            "UPDATE insert_targets SET outcome=3 WHERE batch=?1 AND outcome=2",
            [batch.0]
        )? == removed,
        "compensated insert targets differ from the removal batch"
    );
    ensure!(
        tx.execute(
            "UPDATE insert_batches SET state=6 WHERE id=?1 AND state=5",
            [batch.0]
        )? == 1,
        "compensated insert batch CAS differs"
    );
    Ok(())
}

/// A verified target's first binding version: ours, inserted by a batch.
fn bind_target(
    tx: &Connection,
    content: &Content,
    evidence: &CompletionEvidence,
) -> Result<BindingId> {
    bindings::capacity(tx, true, 0)?;
    let bytes = encode_content(content)?;
    tx.execute(
        "INSERT INTO binding_records(audio,track,path,first_attempt,provenance) VALUES(?1,?2,?3,NULL,2)",
        params![
            content.source.audio.sha256,
            content.track_pid.to_be_bytes(),
            content.path
        ],
    )
    .context("verified binding conflicts with existing content")?;
    let binding = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO binding_versions VALUES(?1,1,?2,?3,?4,NULL,3)",
        params![
            binding,
            bytes,
            evidence.receipt_sha256,
            evidence.media_anchor
        ],
    )?;
    tx.execute(
        "INSERT INTO active_bindings VALUES(?1,?2,1)",
        params![content.source.audio.sha256, binding],
    )?;
    Ok(BindingId(binding))
}

/// Conversions of active tracks: an insert batch's, or a pulled binding's.
const ACTIVE_CONVERSIONS: &str = "SELECT c.track,c.source_path,c.source_bytes,c.source_sha,c.source_audio,c.ffmpeg FROM (SELECT t.track,c.source_path,c.source_bytes,c.source_sha,c.source_audio,c.ffmpeg FROM insert_conversions c JOIN insert_targets t ON t.batch=c.batch AND t.ordinal=c.ordinal JOIN active_bindings a ON a.binding_id=t.binding_id WHERE t.outcome=1 UNION ALL SELECT r.track,p.source_path,p.source_bytes,p.source_sha,p.source_audio,p.ffmpeg FROM binding_conversions p JOIN binding_records r ON r.id=p.binding_id JOIN active_bindings a ON a.binding_id=p.binding_id) c WHERE 1";

fn read_conversion(r: &rusqlite::Row<'_>) -> Result<(u64, Conversion)> {
    Ok((
        u64::from_be_bytes(array(&r.get::<_, Vec<u8>>(0)?)?),
        Conversion {
            source_path: PathBuf::from(r.get::<_, String>(1)?),
            source_bytes: u64::try_from(r.get::<_, i64>(2)?)?,
            source_sha256: array(&r.get::<_, Vec<u8>>(3)?)?,
            source_audio: array(&r.get::<_, Vec<u8>>(4)?)?,
            ffmpeg: r.get(5)?,
        },
    ))
}

/// Consume one reservation of a batch.
fn consume(tx: &Connection, batch: InsertBatchId, namespace: i64, value: &[u8]) -> Result<()> {
    ensure!(
        tx.execute(
            "UPDATE reservations SET consumed=1 WHERE insert_batch=?1 AND namespace=?2 AND value=?3 AND consumed=0",
            params![batch.0, namespace, value],
        )? == 1,
        "insert batch reservations changed during settlement"
    );
    Ok(())
}

impl Journal {
    fn insert_batch_storage(&self) -> Result<()> {
        self.usable()?;
        Ok(())
    }

    pub(super) fn insert_batch_writer(&self) -> Result<()> {
        self.insert_batch_storage()?;
        ensure!(
            self._read_only_copy.is_none(),
            "read-only journal cannot write insert batch storage"
        );
        Ok(())
    }

    /// A Prepared batch that never started may be abandoned. Started batches
    /// have no cancel: they settle from fresh evidence. Its identities stay
    /// reserved.
    pub fn cancel_insert_batch(&mut self, id: InsertBatchId) -> Result<()> {
        self.insert_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        cancel_insert(&tx, id)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }

    /// Record that a sent batch was proven unapplied by a fresh observation.
    /// Nothing is bound; the identities stay reserved.
    pub fn mark_insert_batch_not_applied(
        &mut self,
        id: InsertBatchId,
        proof_receipt: [u8; 32],
    ) -> Result<()> {
        self.insert_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_not_applied(&tx, id, proof_receipt)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)
    }
}

/// Prepared -> Cancelled in `tx`.
pub(super) fn cancel_insert(tx: &Connection, id: InsertBatchId) -> Result<()> {
    ensure!(
        tx.execute(
            "UPDATE insert_batches SET state=3 WHERE id=?1 AND state=0",
            [id.0]
        )? == 1,
        "only a Prepared insert batch can be cancelled"
    );
    Ok(())
}

/// Started -> NotApplied in `tx`.
pub(super) fn insert_not_applied(
    tx: &Connection,
    id: InsertBatchId,
    proof_receipt: [u8; 32],
) -> Result<()> {
    nonzero_hash(&proof_receipt)?;
    ensure!(
        tx.execute(
            "UPDATE insert_batches SET state=4,result_receipt=?1 WHERE id=?2 AND state=1",
            params![proof_receipt, id.0],
        )? == 1,
        "only a Started insert batch can be marked not applied"
    );
    Ok(())
}
