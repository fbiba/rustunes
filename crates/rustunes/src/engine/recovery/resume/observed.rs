//! A Started single insert or cover, observed fresh before any other route.
//!
//! The snapshot `resume` is given is normally the run's own Before, where the
//! row an interrupted insert left without audio never appears, and an
//! attempt the phone never applied looked like any other uncertain one. A
//! complete capture taken now decides: the anchor still at the prepared
//! revision, nothing of the attempt in the catalogue, no file at its planned
//! path and nothing staged (or only the attempt's own pair, never finished,
//! which is removed) settles it NotApplied; an insert's row without a
//! file location is compensated; a cover whose session ended after the phone
//! took its metadata and before its image arrived is settled Abandoned;
//! anything else goes to the attempt's own route, which verifies what the
//! phone holds. Nothing is ever resent.
use super::*;
use crate::engine::recovery::retained;
use crate::{
    engine::execution::native_insert::{create_trace, durable_write},
    engine::recovery::stale_staging,
    engine::state::audio_index::Absent,
    engine::state::journal::{Attempt, AttemptKind},
};
use anyhow::ensure;
use music_protocol::catalogue::identity;
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::transfer::{MAX_BOUNDED_OPS, Transfer, hex};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, time::Duration};

const STAGING: &str = "iTunes_Control/Sync/Media";
const AIRLOCK: &str = "Airlock/Media";
const ARTWORK: &str = "Airlock/Media/Artwork";

/// What the fresh catalogue shows of a Started insert or cover.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// The anchor never moved and nothing of the attempt is in the catalogue;
    /// the phone checks (no file, nothing staged) come next.
    Unmoved,
    /// The insert's row is in the catalogue without a file location.
    Locationless,
    /// The cover's session moved the anchor by one and gave the track, still
    /// at its path, the new cache ID, but no image arrived for it; the
    /// transcript and staging checks come next.
    Imageless,
    /// The phone took a session: the attempt's own route verifies it.
    Moved,
}

/// `before` is the snapshot `resume` was given, used only when it holds the
/// prepared revision: then an unmoved anchor must also keep its tracks.
pub(super) fn classify(attempt: &Attempt, fresh: &Catalog, before: &Catalog) -> Result<Seen> {
    let prepared = &attempt.prepared;
    let content = &prepared.content;
    let row = fresh
        .tracks()
        .iter()
        .find(|t| t.sync_id == content.track_pid);
    let same_tracks = || {
        let ids = |c: &Catalog| {
            c.tracks()
                .iter()
                .map(|t| t.sync_id)
                .collect::<BTreeSet<_>>()
        };
        before.persisted_media_anchor() != prepared.current_media_anchor
            || ids(before) == ids(fresh)
    };
    if fresh.persisted_media_anchor() != prepared.current_media_anchor {
        let imageless = || {
            content.artwork.as_ref().is_some_and(|art| {
                fresh.persisted_media_anchor() == prepared.current_media_anchor + 1
                    && row.is_some_and(|r| r.afc_path.as_ref() == Some(&content.path))
                    && fresh.artwork().track_cache_ids().get(&content.track_pid)
                        == Some(&art.cache_id)
                    && !fresh.artwork().cached_ids().contains(&art.cache_id)
                    && same_tracks()
            })
        };
        return Ok(match prepared.kind {
            AttemptKind::Insert if row.is_some_and(|r| r.afc_path.is_none()) => Seen::Locationless,
            AttemptKind::Artwork if imageless() => Seen::Imageless,
            _ => Seen::Moved,
        });
    }
    let id = attempt.id.as_i64();
    match prepared.kind {
        AttemptKind::Insert => ensure!(
            row.is_none(),
            "the anchor did not move but track {:016x} is in the catalogue; attempt {id} stays Started",
            content.track_pid
        ),
        AttemptKind::Artwork => {
            let art = content
                .artwork
                .as_ref()
                .context("prepared artwork missing")?;
            ensure!(
                row.is_some()
                    && fresh.artwork().track_cache_ids().get(&content.track_pid)
                        != Some(&art.cache_id)
                    && !fresh.artwork().occupied_cache_ids().contains(&art.cache_id),
                "the anchor did not move but the catalogue holds cover {} of track {:016x}; attempt {id} stays Started",
                art.cache_id,
                content.track_pid
            );
        }
    }
    ensure!(
        same_tracks(),
        "the anchor did not move but the catalogue's tracks changed; attempt {id} stays Started"
    );
    Ok(Seen::Unmoved)
}

/// The outcome of observing a Started insert or cover.
pub(super) enum Observed {
    /// Settled NotApplied or Abandoned; the report is final.
    Settled(Report),
    /// The insert's row has no file location: compensate it.
    Locationless,
    /// The phone took the session; the attempt's own route continues under
    /// `trace`, a new directory beside the observation.
    Applied,
}

/// Capture the phone under `trace/observation` and settle an unapplied
/// attempt. `trace` must be new; it is created here.
pub(super) async fn observe(
    journal_path: &Path,
    identity: &Identity,
    attempt: &Attempt,
    before: &snapshot::Snapshot,
    run_dir: Option<&Path>,
    trace: &Path,
    control: &Control,
) -> Result<Observed> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(trace)
        .context("recovery trace directory must be new")?;
    let run = trace.join("observation");
    create_trace(&run)?;
    let content = &attempt.prepared.content;
    let unfilled = BTreeSet::from([content.track_pid]);
    let absent = Absent::unfilled(&unfilled);
    let snapshot_dir = run.join("snapshot");
    crate::engine::verification::audio_capture::capture_allowing(
        &snapshot_dir,
        absent,
        control,
        device::provider,
    )
    .await?;
    let fresh = snapshot::open_allowing(&snapshot_dir, absent)?;
    let library = identity::parse(&fresh.raw_preferences)?;
    let observed = Identity::for_saved_device(fresh.identity.device_identifier_sha256, &library)?;
    ensure!(
        observed.device_digest() == identity.device_digest()
            && observed.library_digest() == identity.library_digest(),
        "the connected phone or library differs from the journal"
    );
    let seen = classify(attempt, &fresh.catalog, &before.catalog)?;
    match seen {
        Seen::Locationless => return Ok(Observed::Locationless),
        Seen::Moved => return Ok(Observed::Applied),
        Seen::Unmoved | Seen::Imageless => {}
    }
    let deadline = control
        .deadline
        .min(Instant::now() + Duration::from_secs(30));
    let provider = timeout_at(deadline, device::provider())
        .await
        .context("discovery deadline")??;
    ensure!(
        Identity::new(&provider.udid, &library)?.device_digest() == identity.device_digest(),
        "discovered device differs from the journal"
    );
    let afc_trace = run.join("staging");
    create_trace(&afc_trace)?;
    let mut afc = Transfer::connect_bounded_readonly_summary(
        &provider,
        &afc_trace,
        deadline,
        64.min(MAX_BOUNDED_OPS),
        Arc::clone(&control.cancelled),
    )
    .await?;
    let listed = |names: Vec<String>| -> Vec<String> {
        names
            .into_iter()
            .filter(|n| n != "." && n != "..")
            .collect()
    };
    let id = attempt.id.as_i64();
    let staged = listed(afc.names_if_present(STAGING).await?);
    let file_absent = match attempt.prepared.kind {
        AttemptKind::Insert => {
            ensure!(
                afc.stat(&content.path).await?.is_none(),
                "a file exists at {}, where attempt {id} planned its audio; it stays Started",
                content.path
            );
            Some(&content.path)
        }
        AttemptKind::Artwork => None,
    };
    // An upload the phone never claimed stays in the airlock, as after a
    // rolled-back batch; it is named, not removed.
    let asset = (content.track_pid as i64).to_string();
    let ours = |n: &String| n == &asset || n.starts_with(&format!("{asset}."));
    let mut unclaimed: Vec<String> = listed(afc.names_if_present(AIRLOCK).await?)
        .into_iter()
        .filter(ours)
        .collect();
    unclaimed.extend(
        listed(afc.names_if_present(ARTWORK).await?)
            .into_iter()
            .filter(ours)
            .map(|n| format!("Artwork/{n}")),
    );
    drop(afc);
    let kind = match attempt.prepared.kind {
        AttemptKind::Insert => "insert",
        AttemptKind::Artwork => "artwork",
    };
    if seen == Seen::Imageless {
        return abandoned(
            journal_path,
            identity,
            attempt,
            &fresh,
            run_dir,
            &run,
            &staged,
            unclaimed,
            control,
        );
    }
    // A pair a killed host staged and never finished is removed once its
    // session's transcript, in the run that started it, proves that.
    let staged_removed = if staged.is_empty() {
        Vec::new()
    } else {
        async {
            let run_dir = run_dir.context(
                "resume needs --run-dir, the run that started it, to prove the pair its own",
            )?;
            let start = Journal::open_read_only(journal_path, identity)?
                .start_evidence(attempt.id)?
                .context("durable start evidence missing")?;
            stale_staging::remove_unfinished(
                identity,
                &library,
                &run,
                &staged,
                stale_staging::Unfinished {
                    send: &retained::transcript_directory(run_dir, attempt)?,
                    operation: &attempt.prepared.operation,
                    signature: Some(start.signature),
                },
                control,
            )
            .await
        }
        .await
        .with_context(|| {
            format!(
                "the operation is still staged on the phone ({}); attempt {id} stays Started",
                staged.join(", ")
            )
        })?
    };
    let proof = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.attempt-not-applied","attempt":id,
        "kind":kind,
        "track":format!("{:016x}",content.track_pid),"anchor":fresh.catalog.persisted_media_anchor(),
        "after_index_sha256":hex(&fresh.index_receipt_sha256),"target_absent":true,
        "planned_path_absent":file_absent,"staging_empty":true,"staged_removed":staged_removed,
        "uploaded_unclaimed":unclaimed,
        "native_operation_replayed":false,
    }))?;
    durable_write(&run, "not-applied.json", &proof)?;
    control.check()?;
    Journal::open(journal_path, identity)?
        .mark_not_applied(attempt.id, Sha256::digest(&proof).into())?;
    Ok(Observed::Settled(Report {
        kind: Kind::Batch,
        successful: true,
        document: json!({
            "schema":"rustunes.attempt-resume","attempt":id,"kind":kind,
            "status":"not_applied","track":format!("{:016x}",content.track_pid),
            "uploaded_unclaimed":unclaimed,"evidence":run,"native_operation_replayed":false,
        }),
    }))
}

/// Settle an imageless cover Abandoned: its transcript, in the run that
/// started it, shows FinishedSyncingMetadata sent and no upload completion
/// after it, so no image is coming, and the phone holds nothing staged.
#[allow(clippy::too_many_arguments)]
fn abandoned(
    journal_path: &Path,
    identity: &Identity,
    attempt: &Attempt,
    fresh: &snapshot::Snapshot,
    run_dir: Option<&Path>,
    run: &Path,
    staged: &[String],
    unclaimed: Vec<String>,
    control: &Control,
) -> Result<Observed> {
    let id = attempt.id.as_i64();
    let content = &attempt.prepared.content;
    let art = content
        .artwork
        .as_ref()
        .context("prepared artwork missing")?;
    let stays = || {
        format!(
            "the phone took cover attempt {id}'s metadata (track {:016x} holds cache ID {}) but no image; it stays Started",
            content.track_pid, art.cache_id
        )
    };
    ensure!(
        staged.is_empty(),
        "{}: its staging folder still holds {}",
        stays(),
        staged.join(", ")
    );
    let run_dir = run_dir.with_context(|| {
        format!(
            "{}: resume needs --run-dir, the run that started it, to read its session",
            stays()
        )
    })?;
    let commands =
        stale_staging::outgoing_commands(&retained::transcript_directory(run_dir, attempt)?)
            .with_context(stays)?;
    let sent = |name: &str| commands.iter().any(|command| command == name);
    ensure!(
        sent("FinishedSyncingMetadata") && !sent("FileComplete") && !sent("FileError"),
        "{}: its session did not end between FinishedSyncingMetadata and the cover's completion",
        stays()
    );
    let anchor = fresh.catalog.persisted_media_anchor();
    let proof = serde_json::to_vec_pretty(&json!({
        "schema":"rustunes.attempt-abandoned","attempt":id,"kind":"artwork",
        "track":format!("{:016x}",content.track_pid),"cache_id":art.cache_id,"anchor":anchor,
        "after_index_sha256":hex(&fresh.index_receipt_sha256),"track_holds_cache_id":true,
        "image_absent":true,"outgoing_commands":commands,"staging_empty":true,
        "uploaded_unclaimed":unclaimed,"native_operation_replayed":false,
    }))?;
    durable_write(run, "abandoned.json", &proof)?;
    control.check()?;
    Journal::open(journal_path, identity)?.mark_abandoned(
        attempt.id,
        Sha256::digest(&proof).into(),
        anchor,
    )?;
    Ok(Observed::Settled(Report {
        kind: Kind::Batch,
        successful: true,
        document: json!({
            "schema":"rustunes.attempt-resume","attempt":id,"kind":"artwork",
            "status":"abandoned","track":format!("{:016x}",content.track_pid),
            "cache_id":art.cache_id,"uploaded_unclaimed":unclaimed,"evidence":run,
            "native_operation_replayed":false,
        }),
    }))
}

#[cfg(test)]
mod tests;
