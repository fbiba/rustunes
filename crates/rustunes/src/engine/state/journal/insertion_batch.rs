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
