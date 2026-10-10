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

/// The row checks of every batch and route table.
pub(super) fn validate_storage(c: &Connection) -> Result<()> {
    validate_attempt_sequence(c)?;
    bindings::validate(c)?;
    ensure!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts a WHERE a.state=4 AND (SELECT count(*) FROM removal_batches b WHERE b.compensates=a.id AND b.state=2)!=1)",
            [],
            |r| r.get::<_, bool>(0)
        )?,
        "Compensated parent lacks exactly one verified compensation"
    );
    coordination::validate_rows(c)?;
    validate_rows(c)?;
    metadata_batch::validate_rows(c)?;
    validate_exclusion(c)
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

fn load_batch(c: &Connection, id: InsertBatchId) -> Result<InsertBatchRecord> {
    type Raw = (
        Option<i64>,
        i64,
        Vec<u8>,
        Vec<u8>,
        u32,
        [Vec<u8>; 3],
        String,
        (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>),
        Option<u32>,
    );
    let raw: Raw = c
        .query_row(
            "SELECT coordination,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence,ready_receipt,signature,result_receipt,verified_anchor FROM insert_batches WHERE id=?1",
            [id.0],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    [r.get(5)?, r.get(6)?, r.get(7)?],
                    r.get(8)?,
                    (r.get(9)?, r.get(10)?, r.get(11)?),
                    r.get(12)?,
                ))
            },
        )
        .optional()?
        .context("insert batch not found")?;
    ensure!(
        coordination::digest(&raw.2) == array::<32>(&raw.3)?,
        "insert batch operation digest differs"
    );
    let mut targets = Vec::new();
    {
        let mut query = c.prepare("SELECT t.ordinal,t.track,t.path,t.content,t.new_album,t.new_artist,t.outcome,t.binding_id,t.binding_version,c.source_path,c.source_bytes,c.source_sha,c.source_audio,c.ffmpeg,s.source FROM insert_targets t LEFT JOIN insert_conversions c ON c.batch=t.batch AND c.ordinal=t.ordinal LEFT JOIN insert_sources s ON s.batch=t.batch AND s.ordinal=t.ordinal WHERE t.batch=?1 ORDER BY t.ordinal")?;
        let mut rows = query.query([id.0])?;
        while let Some(r) = rows.next()? {
            ensure!(
                targets.len() < coordination::MAX_TARGETS,
                "insert batch target bound"
            );
            ensure!(
                usize::try_from(r.get::<_, i64>(0)?)? == targets.len(),
                "insert batch targets have a gap"
            );
            let content = decode_content(&r.get::<_, Vec<u8>>(3)?)?;
            ensure!(
                content.track_pid.to_be_bytes().as_slice() == r.get::<_, Vec<u8>>(1)?
                    && content.path == r.get::<_, String>(2)?,
                "insert target key differs from its content"
            );
            let pid = |n: usize| -> Result<Option<u64>> {
                r.get::<_, Option<Vec<u8>>>(n)?
                    .map(|b| Ok(u64::from_be_bytes(array(&b)?)))
                    .transpose()
            };
            let binding: Option<i64> = r.get(7)?;
            let conversion = r
                .get::<_, Option<String>>(9)?
                .map(|path| {
                    Ok::<_, anyhow::Error>(Conversion {
                        source_path: PathBuf::from(path),
                        source_bytes: u64::try_from(r.get::<_, i64>(10)?)?,
                        source_sha256: array(&r.get::<_, Vec<u8>>(11)?)?,
                        source_audio: array(&r.get::<_, Vec<u8>>(12)?)?,
                        ffmpeg: r.get(13)?,
                    })
                })
                .transpose()?;
            targets.push(InsertTargetRecord {
                target: InsertTarget {
                    content,
                    new_album_pid: pid(4)?,
                    new_artist_pid: pid(5)?,
                    conversion,
                    source: r.get::<_, Option<String>>(14)?.map(PathBuf::from),
                },
                outcome: TargetOutcome::decode(r.get(6)?)?,
                binding: binding
                    .map(|b| Ok::<_, anyhow::Error>((BindingId(b), r.get(8)?)))
                    .transpose()?,
            });
        }
    }
    let [before_current, before_index, input_receipt] = raw.5;
    let (ready, signature, result) = raw.7;
    let record = InsertBatchRecord {
        id,
        coordination: raw.0.map(CoordinationId),
        state: InsertBatchState::decode(raw.1)?,
        operation: raw.2,
        base_anchor: raw.4,
        before_current: array(&before_current)?,
        before_index: array(&before_index)?,
        input_receipt: array(&input_receipt)?,
        evidence: PathBuf::from(raw.6),
        ready_receipt: ready.as_deref().map(array).transpose()?,
        signature: signature.as_deref().map(array).transpose()?,
        result_receipt: result.as_deref().map(array).transpose()?,
        verified_anchor: raw.8,
        targets,
    };
    if record.state != InsertBatchState::Cancelled || !record.targets.is_empty() {
        validate_intent(&InsertIntent {
            coordination: record.coordination,
            operation: record.operation.clone(),
            base_anchor: record.base_anchor,
            before_current: record.before_current,
            before_index: record.before_index,
            input_receipt: record.input_receipt,
            evidence: record.evidence.clone(),
            targets: record.targets.iter().map(|t| t.target.clone()).collect(),
        })?;
    }
    for receipt in [&record.ready_receipt, &record.result_receipt]
        .into_iter()
        .flatten()
    {
        nonzero_hash(receipt)?;
    }
    if let Some(signature) = &record.signature {
        ensure!(signature[0] == 1, "invalid insert batch signature envelope");
    }
    Ok(record)
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

/// Cross-table facts of one batch; its own cells were checked by `load_batch`.
fn validate_batch(c: &Connection, record: &InsertBatchRecord) -> Result<()> {
    use InsertBatchState as S;
    ensure!(
        record.state == S::Cancelled || !record.targets.is_empty(),
        "a sent or settled insert batch has no targets"
    );
    let outcomes = |wanted: &[Option<TargetOutcome>]| {
        record.targets.iter().all(|t| wanted.contains(&t.outcome))
    };
    let has = |wanted: TargetOutcome| record.targets.iter().any(|t| t.outcome == Some(wanted));
    let bound = Some(TargetOutcome::Bound);
    let pending = Some(TargetOutcome::PendingCompensation);
    let compensated = Some(TargetOutcome::Compensated);
    let removed = Some(TargetOutcome::RemovedByPhone);
    ensure!(
        match record.state {
            S::Prepared | S::Started | S::Cancelled | S::NotApplied => outcomes(&[None]),
            S::Verified => outcomes(&[bound]),
            S::Partial => {
                outcomes(&[bound, pending, removed]) && has(TargetOutcome::PendingCompensation)
            }
            S::Compensated => {
                outcomes(&[bound, compensated, removed])
                    && (has(TargetOutcome::Compensated) || has(TargetOutcome::RemovedByPhone))
            }
        },
        "insert batch state differs from its target outcomes"
    );
    if matches!(record.state, S::Verified | S::Partial | S::Compensated) {
        ensure!(
            record
                .verified_anchor
                .is_some_and(|anchor| anchor > record.base_anchor),
            "verified insert anchor does not cover its revision"
        );
    }
    let step: Option<i64> = c
        .query_row(
            "SELECT coordination FROM coordination_steps WHERE insert_batch=?1",
            [record.id.0],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        step == record.coordination.map(|id| id.as_i64()),
        "insert batch coordination differs from its step"
    );
    for (ordinal, t) in record.targets.iter().enumerate() {
        match (t.outcome, t.binding) {
            (Some(TargetOutcome::Bound), Some((binding, 1))) => {
                validate_binding(c, record, &t.target.content, binding)
                    .with_context(|| format!("insert target {ordinal}"))?
            }
            (Some(TargetOutcome::Bound), _) => {
                bail!("a bound insert target has no first binding version")
            }
            (_, None) => {}
            (_, Some(_)) => bail!("an unbound insert target names a binding"),
        }
        if t.outcome != Some(TargetOutcome::Bound) {
            ensure!(
                !c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM binding_records WHERE track=?1 OR path=?2 COLLATE NOCASE)",
                    params![t.target.content.track_pid.to_be_bytes(), t.target.content.path],
                    |r| r.get::<_, bool>(0)
                )?,
                "an unbound insert target owns a binding"
            );
        }
    }
    let actual: BTreeMap<(i64, Vec<u8>), bool> = c
        .prepare("SELECT namespace,value,consumed FROM reservations WHERE insert_batch=?1")?
        .query_map([record.id.0], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        actual == expected_reservations(record),
        "insert batch reservations differ from its targets"
    );
    Ok(())
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

/// Every insert batch, plus the facts that join them.
fn validate_rows(c: &Connection) -> Result<()> {
    ensure!(
        c.query_row("SELECT count(*) FROM insert_batches", [], |r| r
            .get::<_, i64>(0))?
            <= coordination::MAX_BATCHES,
        "insert batch history bound"
    );
    let ids = c
        .prepare("SELECT id FROM insert_batches ORDER BY id")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let record = load_batch(c, InsertBatchId(id))?;
        validate_batch(c, &record)?;
    }
    ensure!(
        !c.query_row(
            "SELECT EXISTS(SELECT 1 FROM binding_records r WHERE r.provenance=2 AND NOT EXISTS(SELECT 1 FROM insert_targets t WHERE t.binding_id=r.id AND t.outcome=1))",
            [],
            |r| r.get::<_, bool>(0)
        )?,
        "an insert-batch binding has no bound insert target"
    );
    ensure!(
        !c.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM ({ACTIVE_CONVERSIONS}) GROUP BY source_audio HAVING count(*)>1)"),
            [],
            |r| r.get::<_, bool>(0)
        )?,
        "a FLAC is linked to two active tracks"
    );
    Ok(())
}

/// A compensating removal batch removes exactly the locationless rows of its
/// insert batch, and the two states agree.
pub(super) fn validate_compensation(
    c: &Connection,
    parent: InsertBatchId,
    record: &coordination::BatchRecord,
) -> Result<()> {
    use coordination::BatchState as R;
    let state: i64 = c.query_row(
        "SELECT state FROM insert_batches WHERE id=?1",
        [parent.0],
        |r| r.get(0),
    )?;
    match record.state {
        R::Verified => ensure!(state == 6, "compensating batch parent differs"),
        R::Prepared | R::Started => ensure!(state == 5, "compensating batch parent is not Partial"),
        R::Cancelled | R::NotApplied => {}
    }
    let owed: BTreeSet<u64> = c
        .prepare("SELECT track FROM insert_targets WHERE batch=?1 AND outcome IN(2,3)")?
        .query_map([parent.0], |r| r.get::<_, Vec<u8>>(0))?
        .map(|b| Ok(u64::from_be_bytes(array(&b?)?)))
        .collect::<Result<_>>()?;
    let removed: BTreeSet<u64> = record.targets.iter().map(|t| t.track).collect();
    ensure!(
        owed == removed && removed.len() == record.targets.len(),
        "a compensating batch removes exactly the locationless rows of its insert batch"
    );
    Ok(())
}

/// A removal batch may be prepared while an insert batch is outstanding only to
/// compensate exactly that Partial batch, or as the removal half of that
/// Prepared batch's session (`session`), and never beside a metadata batch.
pub(super) fn ensure_removal_admitted(
    c: &Connection,
    intent: &coordination::RemovalIntent,
    session: Option<InsertBatchId>,
) -> Result<()> {
    ensure!(
        metadata_batch::outstanding_id(c)?.is_none(),
        metadata_batch::OUTSTANDING_BATCH
    );
    let outstanding = outstanding_id(c)?;
    if let Some(batch) = session {
        ensure!(
            outstanding == Some(batch.0)
                && load_batch(c, batch)?.state == InsertBatchState::Prepared,
            "a session's removal half needs its Prepared insert half as the only outstanding insert batch"
        );
        return Ok(());
    }
    let Some(batch) = intent.compensates_insert_batch else {
        ensure!(outstanding.is_none(), OUTSTANDING_BATCH);
        return Ok(());
    };
    ensure!(
        outstanding == Some(batch.0),
        "a compensating batch needs its Partial insert batch as the only outstanding insert batch"
    );
    let record = load_batch(c, batch)?;
    ensure!(
        record.state == InsertBatchState::Partial,
        "a compensating batch needs a Partial insert batch"
    );
    let owed: BTreeSet<u64> = record
        .targets
        .iter()
        .filter(|t| t.outcome == Some(TargetOutcome::PendingCompensation))
        .map(|t| t.target.content.track_pid)
        .collect();
    let removed: BTreeSet<u64> = intent.targets.iter().map(|t| t.track).collect();
    ensure!(
        owed == removed,
        "a compensating batch removes exactly the locationless rows of its insert batch"
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

/// The active track converted from the FLAC with `audio`, if any.
fn active_conversion(c: &Connection, audio: &[u8; 32]) -> Result<Option<u64>> {
    let mut query = c.prepare(&format!("{ACTIVE_CONVERSIONS} AND c.source_audio=?1"))?;
    let mut rows = query.query([audio])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let (track, _) = read_conversion(row)?;
    ensure!(
        rows.next()?.is_none(),
        "a FLAC is linked to two active tracks"
    );
    Ok(Some(track))
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

    pub fn insert_batch(&self, id: InsertBatchId) -> Result<InsertBatchRecord> {
        self.insert_batch_storage()?;
        load_batch(&self.connection, id)
    }

    /// Every insert batch, oldest first.
    #[cfg(test)]
    pub fn insert_batches(&self) -> Result<Vec<InsertBatchRecord>> {
        self.usable()?;
        let ids = self
            .connection
            .prepare("SELECT id FROM insert_batches ORDER BY id")?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| load_batch(&self.connection, InsertBatchId(id)))
            .collect()
    }

    /// Each track whose active binding an insert batch target created, with
    /// that batch and the target's ordinal.
    pub fn bound_insert_targets(&self) -> Result<BTreeMap<u64, (InsertBatchId, usize)>> {
        self.usable()?;
        let mut statement = self.connection.prepare(
            "SELECT t.track,t.batch,t.ordinal FROM insert_targets t JOIN active_bindings a ON a.binding_id=t.binding_id WHERE t.outcome=1",
        )?;
        let mut rows = statement.query([])?;
        let mut targets = BTreeMap::new();
        while let Some(r) = rows.next()? {
            let track = u64::from_be_bytes(array(&r.get::<_, Vec<u8>>(0)?)?);
            let target = (
                InsertBatchId(r.get(1)?),
                usize::try_from(r.get::<_, i64>(2)?)?,
            );
            ensure!(
                targets.insert(track, target).is_none(),
                "a track has two bound insert targets"
            );
        }
        Ok(targets)
    }

    /// The FLAC each active track converted from one came from, by track.
    pub fn conversions(&self) -> Result<BTreeMap<u64, Conversion>> {
        self.usable()?;
        let mut statement = self.connection.prepare(ACTIVE_CONVERSIONS)?;
        let mut rows = statement.query([])?;
        let mut conversions = BTreeMap::new();
        let mut audio = BTreeSet::new();
        while let Some(r) = rows.next()? {
            let (track, conversion) = read_conversion(r)?;
            ensure!(
                audio.insert(conversion.source_audio),
                "a FLAC is linked to two active tracks"
            );
            ensure!(
                conversions.insert(track, conversion).is_none(),
                "a track has two conversions"
            );
        }
        Ok(conversions)
    }

    /// The single Prepared, Started or Partial batch, if any.
    pub fn outstanding_insert_batch(&self) -> Result<Option<InsertBatchRecord>> {
        self.usable()?;
        outstanding_id(&self.connection)?
            .map(|id| load_batch(&self.connection, InsertBatchId(id)))
            .transpose()
    }

    #[cfg(test)]
    pub fn insert_batch_counts(&self) -> Result<InsertBatchCounts> {
        self.usable()?;
        let mut counts = InsertBatchCounts::default();
        let mut query = self
            .connection
            .prepare("SELECT state,count(*) FROM insert_batches GROUP BY state")?;
        let rows = query
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (state, n) in rows {
            let n = u64::try_from(n)?;
            match InsertBatchState::decode(state)? {
                InsertBatchState::Prepared => counts.prepared = n,
                InsertBatchState::Started => counts.started = n,
                InsertBatchState::Verified => counts.verified = n,
                InsertBatchState::Cancelled => counts.cancelled = n,
                InsertBatchState::NotApplied => counts.not_applied = n,
                InsertBatchState::Partial => counts.partial = n,
                InsertBatchState::Compensated => counts.compensated = n,
            }
        }
        Ok(counts)
    }

    /// Durable local intent only, with its targets and (for a coordinated
    /// batch) the coordination's next step in one transaction. This grants no
    /// native permit. Every track, path and new collection becomes reserved for
    /// good, exactly as for an Insert attempt.
    pub fn prepare_insert_batch(&mut self, intent: &InsertIntent) -> Result<InsertBatchId> {
        self.insert_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = prepare_insert(&tx, intent)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(id)
    }

    /// Durable Prepared -> Started before the signed operation is sent. There
    /// is no retry: a lost `StartedInsertBatch` requires fresh reconciliation.
    pub fn start_insert_batch(
        &mut self,
        id: InsertBatchId,
        ready_receipt: [u8; 32],
        signature: [u8; 21],
    ) -> Result<StartedInsertBatch> {
        self.insert_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let started = start_insert(&tx, id, ready_receipt, signature)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(started)
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

    /// Settle a Started batch from fresh evidence. `bound` lists, by target
    /// ordinal, the completion evidence of every target that verified; each is
    /// checked against its Content exactly as an Insert attempt's completion is,
    /// then bound as an active binding and its reservations consumed. Each
    /// target in `removed` was deleted again by the phone and
    /// becomes RemovedByPhone; every other target becomes PendingCompensation.
    /// Unbound targets keep their reservations. All of it is one transaction:
    /// the batch is Verified when every target is bound, Compensated when every
    /// other target was removed by the phone, otherwise Partial, which stays
    /// outstanding until a compensating removal batch settles.
    pub fn settle_insert_batch(
        &mut self,
        id: InsertBatchId,
        result_receipt: [u8; 32],
        verified_anchor: u32,
        bound: &[(usize, CompletionEvidence)],
        removed: &[usize],
    ) -> Result<InsertBatchState> {
        self.insert_batch_writer()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = settle_insert(&tx, id, result_receipt, verified_anchor, bound, removed)?;
        validate_schema(&tx)?;
        durable_commit(tx, &self.database, &self.directory, &mut self.poisoned)?;
        Ok(state)
    }
}

/// Prepare `intent` in `tx`, as `Journal::prepare_insert_batch` does.
pub(super) fn prepare_insert(tx: &Connection, intent: &InsertIntent) -> Result<InsertBatchId> {
    validate_intent(intent)?;
    let evidence = coordination::evidence_text(&intent.evidence)?;
    let contents = intent
        .targets
        .iter()
        .map(|t| encode_content(&t.content))
        .collect::<Result<Vec<_>>>()?;
    match intent.coordination {
        Some(id) => ensure!(
            coordination::active_id(tx)? == Some(id.as_i64()),
            "insert batch names a coordination that is not Active"
        ),
        None => coordination::ensure_no_route_owner(tx)?,
    }
    coordination::ensure_quiescent(tx)?;
    for target in &intent.targets {
        let content = &target.content;
        ensure!(
            lookup(tx, &content.source.audio.sha256)?.is_none(),
            "encoded audio is already bound; do not reinsert"
        );
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM binding_records WHERE track=?1 OR path=?2 COLLATE NOCASE)",
                params![content.track_pid.to_be_bytes(), content.path],
                |r| r.get::<_, bool>(0)
            )?,
            "track identity or path already belongs to binding history"
        );
        if let Some(conversion) = &target.conversion {
            // A FLAC is on the phone once: as itself, or as one ALAC.
            ensure!(
                lookup(tx, &conversion.source_audio)?.is_none()
                    && active_conversion(tx, &conversion.source_audio)?.is_none(),
                "this FLAC's audio is already on the phone; do not convert it again"
            );
        }
    }
    let new_bindings = i64::try_from(intent.targets.len())?;
    let (records, versions): (i64, i64) = tx.query_row(
        "SELECT (SELECT count(*) FROM binding_records),(SELECT count(*) FROM binding_versions)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        versions + new_bindings < 100_000 && records + new_bindings <= 50_000,
        "journal binding history capacity exhausted"
    );
    bindings::database_bound(tx, 4 * 1024 * 1024)?;
    tx.execute(
        "INSERT INTO insert_batches(coordination,state,operation,operation_sha,base_anchor,before_current,before_index,input_receipt,evidence) VALUES(?1,0,?2,?3,?4,?5,?6,?7,?8)",
        params![
            intent.coordination.map(|c| c.as_i64()),
            intent.operation,
            coordination::digest(&intent.operation),
            intent.base_anchor,
            intent.before_current,
            intent.before_index,
            intent.input_receipt,
            evidence
        ],
    )?;
    let id = InsertBatchId(tx.last_insert_rowid());
    for pid in reserved_pids(&intent.targets) {
        reserve_for(tx, 1, &pid.to_be_bytes(), None, Some(id.0), false)?;
    }
    for target in &intent.targets {
        reserve_for(
            tx,
            3,
            target.content.path.to_ascii_lowercase().as_bytes(),
            None,
            Some(id.0),
            false,
        )?;
    }
    for cache in reserved_caches(&intent.targets) {
        reserve_for(tx, 2, &cache.to_be_bytes(), None, Some(id.0), false)?;
    }
    for (ordinal, (target, content)) in intent.targets.iter().zip(&contents).enumerate() {
        tx.execute(
            "INSERT INTO insert_targets(batch,ordinal,track,path,content,new_album,new_artist) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id.0,
                i64::try_from(ordinal)?,
                target.content.track_pid.to_be_bytes(),
                target.content.path,
                content,
                target.new_album_pid.map(u64::to_be_bytes),
                target.new_artist_pid.map(u64::to_be_bytes)
            ],
        )?;
        if let Some(source) = &target.source {
            tx.execute(
                "INSERT INTO insert_sources VALUES(?1,?2,?3)",
                params![
                    id.0,
                    i64::try_from(ordinal)?,
                    source.to_str().context("insert source path is not UTF-8")?
                ],
            )?;
        }
        if let Some(conversion) = &target.conversion {
            tx.execute(
                "INSERT INTO insert_conversions VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    id.0,
                    i64::try_from(ordinal)?,
                    conversion
                        .source_path
                        .to_str()
                        .context("conversion source path is not UTF-8")?,
                    i64::try_from(conversion.source_bytes)?,
                    conversion.source_sha256,
                    conversion.source_audio,
                    conversion.ffmpeg
                ],
            )?;
        }
    }
    if let Some(coordination) = intent.coordination {
        coordination::link_step(tx, coordination, Work::Insert(id))?;
    }
    bindings::database_bound(tx, 4 * 1024 * 1024)?;
    Ok(id)
}

/// Prepared -> Started in `tx`, as `Journal::start_insert_batch` does.
pub(super) fn start_insert(
    tx: &Connection,
    id: InsertBatchId,
    ready_receipt: [u8; 32],
    signature: [u8; 21],
) -> Result<StartedInsertBatch> {
    nonzero_hash(&ready_receipt)?;
    ensure!(signature[0] == 1, "unsupported metadata signature envelope");
    let batch = load_batch(tx, id)?;
    ensure!(
        batch.state == InsertBatchState::Prepared,
        "insert batch already started or closed; reconcile instead of retrying"
    );
    ensure!(
        tx.execute(
            "UPDATE insert_batches SET state=1,ready_receipt=?1,signature=?2 WHERE id=?3 AND state=0",
            params![ready_receipt, signature.as_slice(), id.0],
        )? == 1,
        "insert batch start CAS differs"
    );
    bindings::database_bound(tx, 4 * 1024 * 1024)?;
    Ok(StartedInsertBatch {
        operation: batch.operation,
        signature,
    })
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

/// Settle a Started batch in `tx`, as `Journal::settle_insert_batch` does.
pub(super) fn settle_insert(
    tx: &Connection,
    id: InsertBatchId,
    result_receipt: [u8; 32],
    verified_anchor: u32,
    bound: &[(usize, CompletionEvidence)],
    removed: &[usize],
) -> Result<InsertBatchState> {
    nonzero_hash(&result_receipt)?;
    let batch = load_batch(tx, id)?;
    ensure!(
        batch.state == InsertBatchState::Started,
        "insert batch is not Started"
    );
    ensure!(
        verified_anchor > batch.base_anchor,
        "verified insert does not cover its revision"
    );
    let mut evidence = BTreeMap::new();
    for (ordinal, claim) in bound {
        let target = batch
            .targets
            .get(*ordinal)
            .context("insert settlement names a target outside the batch")?;
        ensure!(
            evidence.insert(*ordinal, claim).is_none(),
            "insert settlement names a target twice"
        );
        verify_content(&target.target.content, claim)?;
        ensure!(
            claim.media_anchor == verified_anchor,
            "verified target anchor differs from the batch anchor"
        );
    }
    let mut gone = BTreeSet::new();
    for ordinal in removed {
        ensure!(
            *ordinal < batch.targets.len(),
            "insert settlement names a target outside the batch"
        );
        ensure!(
            !evidence.contains_key(ordinal) && gone.insert(*ordinal),
            "insert settlement names a target twice"
        );
    }
    let state = if evidence.len() == batch.targets.len() {
        InsertBatchState::Verified
    } else if evidence.len() + gone.len() == batch.targets.len() {
        InsertBatchState::Compensated
    } else {
        InsertBatchState::Partial
    };
    let mut collections = BTreeSet::new();
    for (ordinal, target) in batch.targets.iter().enumerate() {
        let content = &target.target.content;
        let ordinal_row = i64::try_from(ordinal)?;
        let Some(claim) = evidence.get(&ordinal) else {
            ensure!(
                tx.execute(
                    "UPDATE insert_targets SET outcome=?1 WHERE batch=?2 AND ordinal=?3 AND outcome IS NULL",
                    params![if gone.contains(&ordinal) { 4 } else { 2 }, id.0, ordinal_row],
                )? == 1,
                "insert target outcome CAS differs"
            );
            continue;
        };
        let binding = bind_target(tx, content, claim)?;
        tx.execute(
            "INSERT INTO binding_sources SELECT ?1,source FROM insert_sources WHERE batch=?2 AND ordinal=?3",
            params![binding.0, id.0, ordinal_row],
        )?;
        ensure!(
            tx.execute(
                "UPDATE insert_targets SET outcome=1,binding_id=?1,binding_version=1 WHERE batch=?2 AND ordinal=?3 AND outcome IS NULL",
                params![binding.as_i64(), id.0, ordinal_row],
            )? == 1,
            "insert target outcome CAS differs"
        );
        consume(tx, id, 1, &content.track_pid.to_be_bytes())?;
        consume(tx, id, 3, content.path.to_ascii_lowercase().as_bytes())?;
        if let Some(artwork) = &content.artwork {
            consume(tx, id, 2, &artwork.cache_id.to_be_bytes())?;
        }
        collections.extend(
            [target.target.new_album_pid, target.target.new_artist_pid]
                .into_iter()
                .flatten(),
        );
    }
    for pid in collections {
        consume(tx, id, 1, &pid.to_be_bytes())?;
    }
    ensure!(
        tx.execute(
            "UPDATE insert_batches SET state=?1,result_receipt=?2,verified_anchor=?3 WHERE id=?4 AND state=1",
            params![
                match state {
                    InsertBatchState::Verified => 2,
                    InsertBatchState::Compensated => 6,
                    _ => 5,
                },
                result_receipt,
                verified_anchor,
                id.0
            ],
        )? == 1,
        "insert batch verification CAS differs"
    );
    Ok(state)
}
