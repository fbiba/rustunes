//! Row-level validation of attempts, bindings, reservations and the attempt
//! sequence, independent of the SQL constraints.
use super::*;

type TransitionEvidence = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<u32>,
);

pub(super) fn validate_rows(c: &Connection) -> Result<()> {
    let mut q = c.prepare("SELECT id FROM attempts ORDER BY id LIMIT 100001")?;
    let mut rows = q.query([])?;
    let mut count = 0;
    while let Some(r) = rows.next()? {
        count += 1;
        ensure!(count <= 100_000, "attempt bound exceeded");
        let id = AttemptId(r.get(0)?);
        let attempt = load_attempt(c, id)?;
        let (ready, signature, verified, anchor): TransitionEvidence = c.query_row("SELECT ready_receipt,signature,verified_receipt,verified_anchor FROM attempts WHERE id=?1", [id.0], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if matches!(
            attempt.state,
            AttemptState::ReconciliationRequired
                | AttemptState::Verified
                | AttemptState::Compensated
                | AttemptState::NotApplied
                | AttemptState::Abandoned
        ) {
            nonzero_hash(&array::<32>(&ready.context("missing Ready evidence")?)?)?;
            let sig: [u8; 21] = array(&signature.context("missing signature")?)?;
            ensure!(sig[0] == 1, "invalid stored signature envelope");
        }
        if attempt.state == AttemptState::NotApplied {
            nonzero_hash(&array::<32>(
                verified.as_ref().context("missing not-applied receipt")?,
            )?)?;
            ensure!(
                anchor.is_none(),
                "a not-applied attempt has no verified anchor"
            );
        }
        if attempt.state == AttemptState::Abandoned {
            nonzero_hash(&array::<32>(
                verified.as_ref().context("missing abandonment receipt")?,
            )?)?;
            ensure!(
                attempt.prepared.kind == AttemptKind::Artwork
                    && anchor.context("missing abandoned anchor")?
                        > attempt.prepared.current_media_anchor,
                "an abandoned attempt is a cover the phone took at a later anchor"
            );
        }
        if attempt.state == AttemptState::Verified {
            nonzero_hash(&array::<32>(
                &verified.context("missing verified receipt")?,
            )?)?;
            ensure!(
                anchor.context("missing verified anchor")? > attempt.prepared.current_media_anchor,
                "verified anchor does not cover attempt"
            );
        }
    }
    let mut q = c.prepare(&binding_query(" LIMIT 50001"))?;
    let mut rows = q.query([])?;
    let mut count = 0;
    while let Some(r) = rows.next()? {
        count += 1;
        ensure!(count <= 50_000, "binding bound exceeded");
        decode_binding(read_binding(r)?)?;
    }
    ensure!(
        c.query_row("PRAGMA foreign_key_check", [], |r| r.get::<_, String>(0))
            .optional()?
            .is_none(),
        "journal foreign key check failed"
    );
    let seq: Option<i64> = c
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='attempts'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let max: i64 = c.query_row("SELECT COALESCE(MAX(id),0) FROM attempts", [], |r| r.get(0))?;
    ensure!(
        seq.unwrap_or(0) >= max && seq.unwrap_or(0) < i64::MAX,
        "invalid attempt sequence"
    );
    let mut q = c.prepare("SELECT namespace,value FROM reservations LIMIT 100001")?;
    let mut rows = q.query([])?;
    let mut count = 0;
    while let Some(r) = rows.next()? {
        count += 1;
        ensure!(count <= 100_000, "reservation bound exceeded");
        let namespace: i64 = r.get(0)?;
        let value: Vec<u8> = r.get(1)?;
        match namespace {
            1 => ensure!(u64::from_be_bytes(array(&value)?) != 0, "zero reserved PID"),
            2 => ensure!(
                u32::from_be_bytes(array(&value)?) >= 100,
                "invalid reserved cache ID"
            ),
            3 => {
                decode_reserved_path(&value)?;
            }
            _ => bail!("invalid reservation namespace"),
        }
    }
    Ok(())
}
