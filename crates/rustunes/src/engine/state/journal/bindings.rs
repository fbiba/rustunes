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

pub(super) fn recorded_sources(c: &Connection) -> Result<std::collections::BTreeMap<u64, PathBuf>> {
    let mut q = c.prepare("SELECT r.track,s.source FROM binding_sources s JOIN binding_records r ON r.id=s.binding_id JOIN active_bindings a ON a.binding_id=s.binding_id LIMIT 50001")?;
    let mut rows = q.query([])?;
    let mut sources = std::collections::BTreeMap::new();
    while let Some(r) = rows.next()? {
        ensure!(
            sources.len() < 50_000,
            "recorded source inventory exceeds limit"
        );
        let track = u64::from_be_bytes(array(&r.get::<_, Vec<u8>>(0)?)?);
        ensure!(
            sources
                .insert(track, PathBuf::from(r.get::<_, String>(1)?))
                .is_none(),
            "two recorded sources name one track"
        );
    }
    Ok(sources)
}

#[cfg(test)]
pub(super) fn history(
    c: &Connection,
    id: BindingId,
    after: u32,
    limit: u32,
) -> Result<Vec<BindingVersion>> {
    ensure!(
        (1..=1000).contains(&limit),
        "history page limit must be 1..1000"
    );
    let mut q = c.prepare("SELECT r.audio,v.content,v.receipt,v.anchor,r.track,r.path,v.receipt_kind,v.version,EXISTS(SELECT 1 FROM active_bindings a WHERE a.binding_id=r.id AND a.version=v.version) FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id WHERE r.id=?1 AND v.version>?2 ORDER BY v.version LIMIT ?3")?;
    let mut rows = q.query(params![id.0, after, limit])?;
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        result.push(BindingVersion {
            version: r.get(7)?,
            binding: decode_binding(read_binding(r)?)?,
            active: r.get(8)?,
        });
    }
    Ok(result)
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

/// Validate immutable history and its exact active/retired projection.
pub(super) fn validate(c: &Connection) -> Result<()> {
    database_bound(c, 0)?;
    rows::validate_rows(c)?;
    ensure!(!c.query_row("SELECT EXISTS(SELECT 1 FROM attempts a LEFT JOIN binding_versions v ON v.attempt=a.id WHERE (a.state=2)!=(v.attempt IS NOT NULL))",[],|r|r.get::<_,bool>(0))?,"native verified attempt/resulting binding version cardinality differs");
    let foreign_key_error: Option<String> = c
        .query_row("PRAGMA foreign_key_check", [], |r| r.get(0))
        .optional()?;
    ensure!(
        foreign_key_error.is_none(),
        "journal foreign key check failed"
    );
    let mut q = c.prepare("SELECT r.id,r.audio,v.content,v.receipt,v.anchor,r.track,r.path,v.version,r.first_attempt,r.provenance,v.attempt,v.receipt_kind FROM binding_records r JOIN binding_versions v ON v.binding_id=r.id ORDER BY r.id,v.version LIMIT 100001")?;
    let mut rows = q.query([])?;
    let mut previous: Option<(i64, u32, Content)> = None;
    let mut count = 0;
    let mut records = 0_u64;
    while let Some(r) = rows.next()? {
        count += 1;
        ensure!(count <= 100_000, "binding history exceeds limit");
        let id: i64 = r.get(0)?;
        let version: u32 = r.get(7)?;
        let raw = RawBinding {
            audio: r.get(1)?,
            content: r.get(2)?,
            receipt: r.get(3)?,
            anchor: r.get(4)?,
            track: r.get(5)?,
            path: r.get(6)?,
            receipt_kind: r.get(11)?,
        };
        let binding = decode_binding(raw)?;
        nonzero_hash(&binding.receipt_sha256)?;
        if let Some((old_id, old_version, old)) = &previous
            && *old_id == id
        {
            ensure!(
                version
                    == old_version
                        .checked_add(1)
                        .context("binding version overflow")?,
                "binding version gap"
            );
            let mut expected = old.clone();
            expected.artwork = binding.content.artwork.clone();
            ensure!(
                expected == binding.content,
                "binding history changed immutable audio fields"
            );
        } else {
            ensure!(version == 1, "binding history does not start at one");
            records += 1;
            ensure!(
                records <= 50_000,
                "binding lifetime inventory exceeds limit"
            );
            let first: Option<i64> = r.get(8)?;
            let provenance: i64 = r.get(9)?;
            ensure!(
                match provenance {
                    1 => first.is_some(),
                    2 | 3 => first.is_none(),
                    _ => false,
                },
                "invalid binding provenance"
            );
        }
        let event: Option<i64> = r.get(10)?;
        if version == 1 {
            ensure!(
                event == r.get::<_, Option<i64>>(8)?,
                "first binding attempt differs"
            );
        }
        if let Some(event) = event {
            let (receipt,anchor,parent,prior):(Vec<u8>,u32,Option<i64>,Option<u32>)=c.query_row("SELECT verified_receipt,verified_anchor,binding_id,binding_version FROM attempts WHERE id=?1",[event],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            ensure!(
                receipt == binding.receipt_sha256 && anchor == binding.media_anchor,
                "binding version receipt differs from attempt"
            );
            let attempt = load_attempt(c, AttemptId(event))?;
            ensure!(
                attempt.state == AttemptState::Verified
                    && attempt.prepared.content == binding.content,
                "binding version does not match verified attempt"
            );
            ensure!(
                if version == 1 {
                    attempt.prepared.kind == AttemptKind::Insert
                        && parent.is_none()
                        && prior.is_none()
                        && binding.receipt_kind == BindingReceiptKind::InsertVerification
                } else {
                    attempt.prepared.kind == AttemptKind::Artwork
                        && parent == Some(id)
                        && prior == Some(version - 1)
                        && binding.receipt_kind == BindingReceiptKind::ArtworkVerification
                },
                "binding version lifecycle/receipt kind differs from attempt"
            );
        } else {
            ensure!(version == 1, "later binding version has no attempt");
            let provenance: i64 = r.get(9)?;
            ensure!(
                match provenance {
                    2 => binding.receipt_kind == BindingReceiptKind::InsertBatchVerification,
                    3 => binding.receipt_kind == BindingReceiptKind::PullVerification,
                    _ => false,
                },
                "unlinked binding receipt kind differs from provenance"
            );
        }
        previous = Some((id, version, binding.content));
    }
    let total = u64::try_from(
        c.query_row("SELECT count(*) FROM binding_records", [], |r| {
            r.get::<_, i64>(0)
        })?,
    )?;
    ensure!(total == records, "binding record has no version");
    ensure!(!c.query_row("SELECT EXISTS(SELECT 1 FROM binding_records r LEFT JOIN active_bindings a ON a.binding_id=r.id LEFT JOIN binding_tombstones t ON t.binding_id=r.id WHERE (a.binding_id IS NULL AND (t.binding_id IS NULL OR t.binding_version!=(SELECT MAX(version) FROM binding_versions v WHERE v.binding_id=r.id))) OR (a.binding_id IS NOT NULL AND (t.binding_id IS NOT NULL OR a.version!=(SELECT MAX(version) FROM binding_versions v WHERE v.binding_id=r.id))))", [], |r| r.get::<_,bool>(0))?, "binding active/retired projection is incomplete or stale");
    Ok(())
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
