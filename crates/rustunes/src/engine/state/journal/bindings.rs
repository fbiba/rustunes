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
