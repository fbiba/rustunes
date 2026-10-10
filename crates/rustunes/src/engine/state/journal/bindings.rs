//! Binding history: records, versions and the active projection. The
//! active-only `bindings` view serves readers that need no history.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BindingId(pub(super) i64);
impl BindingId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
pub struct BindingVersion {
    pub version: u32,
    pub binding: Binding,
    pub active: bool,
}

#[cfg(test)]
pub(super) fn active_id(c: &Connection, audio: &[u8; 32]) -> Result<Option<BindingId>> {
    Ok(c.query_row(
        "SELECT binding_id FROM active_bindings WHERE audio=?1",
        [audio],
        |r| r.get(0),
    )
    .optional()?
    .map(BindingId))
}

pub(super) fn database_bound(c: &Connection, reserve: u64) -> Result<()> {
    let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let bytes = u64::try_from(pages)?
        .checked_mul(u64::try_from(size)?)
        .and_then(|b| b.checked_add(reserve))
        .context("journal size overflow")?;
    ensure!(
        bytes <= MAX_READ_ONLY_DATABASE,
        "journal exceeds read-only size limit"
    );
    Ok(())
}

pub(super) fn capacity(c: &Connection, new_binding: bool, reserve: u64) -> Result<()> {
    let versions: i64 = c.query_row("SELECT count(*) FROM binding_versions", [], |r| r.get(0))?;
    let records: i64 = c.query_row("SELECT count(*) FROM binding_records", [], |r| r.get(0))?;
    ensure!(
        versions < 100_000 && (!new_binding || records < 50_000),
        "journal binding history capacity exhausted"
    );
    // Leave room for bounded completion rows/index splits before issuing intent.
    database_bound(c, reserve)
}

pub(super) fn prepare(
    c: &Connection,
    p: &PreparedAttempt,
    content: &[u8],
    sha: [u8; 32],
) -> Result<()> {
    capacity(c, p.kind == AttemptKind::Insert, 4 * 1024 * 1024)?;
    let binding = if p.kind == AttemptKind::Artwork {
        let (id, version): (i64, u32) = c.query_row(
            "SELECT binding_id,version FROM active_bindings WHERE audio=?1",
            [&p.content.source.audio.sha256],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(version < u32::MAX, "binding version exhausted");
        Some((id, version))
    } else {
        ensure!(!c.query_row("SELECT EXISTS(SELECT 1 FROM binding_records WHERE track=?1 OR path=?2 COLLATE NOCASE)", params![p.content.track_pid.to_be_bytes(),p.content.path], |r| r.get::<_, bool>(0))?, "track identity or path already belongs to binding history");
        None
    };
    c.execute("INSERT INTO attempts(kind,state,content,operation,operation_sha,album,artist,base_anchor,input_receipt,binding_id,binding_version) VALUES(?1,0,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![kind_number(p.kind),content,p.operation,sha,p.new_album_pid.map(u64::to_be_bytes),p.new_artist_pid.map(u64::to_be_bytes),p.current_media_anchor,p.input_receipt_sha256,binding.map(|b|b.0),binding.map(|b|b.1)])?;
    Ok(())
}

pub(super) fn store_binding(
    c: &Connection,
    content: &Content,
    evidence: &CompletionEvidence,
    artwork: bool,
    attempt: AttemptId,
) -> Result<()> {
    capacity(c, !artwork, 0)?;
    let bytes = encode_content(content)?;
    if artwork {
        let (binding, version): (i64, u32) = c.query_row(
            "SELECT binding_id,binding_version FROM attempts WHERE id=?1 AND kind=2",
            [attempt.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let current: (i64, u32) = c.query_row(
            "SELECT binding_id,version FROM active_bindings WHERE audio=?1",
            [&content.source.audio.sha256],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            current == (binding, version),
            "prepared binding version changed"
        );
        let old = lookup(c, &content.source.audio.sha256)?.context("active binding missing")?;
        let mut expected = old.content;
        expected.artwork = content.artwork.clone();
        ensure!(
            expected == *content,
            "artwork changed immutable binding fields"
        );
        let next = version
            .checked_add(1)
            .context("binding version exhausted")?;
        c.execute(
            "INSERT INTO binding_versions VALUES(?1,?2,?3,?4,?5,?6,2)",
            params![
                binding,
                next,
                bytes,
                evidence.receipt_sha256,
                evidence.media_anchor,
                attempt.0
            ],
        )?;
        ensure!(
            c.execute(
                "UPDATE active_bindings SET version=?1 WHERE binding_id=?2 AND version=?3",
                params![next, binding, version]
            )? == 1,
            "active binding changed"
        );
    } else {
        c.execute("INSERT INTO binding_records(audio,track,path,first_attempt,provenance) VALUES(?1,?2,?3,?4,1)", params![content.source.audio.sha256,content.track_pid.to_be_bytes(),content.path,attempt.0])?;
        let binding = c.last_insert_rowid();
        c.execute(
            "INSERT INTO binding_versions VALUES(?1,1,?2,?3,?4,?5,1)",
            params![
                binding,
                bytes,
                evidence.receipt_sha256,
                evidence.media_anchor,
                attempt.0
            ],
        )?;
        c.execute(
            "INSERT INTO active_bindings VALUES(?1,?2,1)",
            params![content.source.audio.sha256, binding],
        )?;
    }
    database_bound(c, 0)
}

pub(super) fn binding_at(
    c: &Connection,
    reference: BindingRef,
    require_active: bool,
) -> Result<Binding> {
    ensure!(
        reference.id.as_i64() > 0 && reference.version > 0,
        "invalid binding reference"
    );
    if require_active {
        ensure!(
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM active_bindings WHERE binding_id=?1 AND version=?2)",
                params![reference.id.as_i64(), reference.version],
                |r| r.get::<_, bool>(0)
            )?,
            "binding reference is not current active version"
        );
    }
    let raw=c.query_row("SELECT r.audio,v.content,v.receipt,v.anchor,r.track,r.path,v.receipt_kind FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id WHERE r.id=?1 AND v.version=?2",params![reference.id.as_i64(),reference.version],read_binding).context("binding reference not found")?;
    decode_binding(raw)
}
