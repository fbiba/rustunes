use crate::{
    engine::execution::device,
    engine::execution::native_insert::Control,
    engine::recovery::retained,
    engine::state::current,
    engine::state::journal::{Identity, Journal},
    engine::state::snapshot,
};
use anyhow::{Context, Result};
use music_protocol::catalogue::identity;
use std::{
    fs,
    future::Future,
    os::unix::fs::DirBuilderExt,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::time::{Instant, timeout_at};

pub struct Options<'a> {
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    pub run_dir: Option<&'a Path>,
    pub trace: &'a Path,
    pub cancel_prepared: Option<i64>,
    pub selected_device: Option<&'a str>,
}

pub async fn run(options: Options<'_>, engine: &crate::engine::Engine) -> Result<Report> {
    engine.check()?;
    // A new explicit invocation gets one budget; phase collection never renews it.
    let outer_control = engine.control(1800);
    // A batch is settled from the journal, whose record names its evidence:
    // the snapshot only identifies the phone, and may be the capture of the
    // run that met the batch, with the rows it left without audio.
    let any = crate::engine::state::audio_index::Absent {
        any_unfilled: true,
        gone: true,
        ..crate::engine::state::audio_index::Absent::NONE
    };
    let saved = snapshot::open_allowing_with_control(options.snapshot, any, &outer_control)?;
    let library = identity::parse(&saved.raw_preferences)?;
    let identity = Identity::for_saved_device(saved.identity.device_identifier_sha256, &library)?;
    if let Some(selected) = options.selected_device {
        let selected = Identity::new(selected, &library)?;
        anyhow::ensure!(
            selected.device_digest() == identity.device_digest(),
            "selected device differs from recovery snapshot"
        );
    }
    let journal = Journal::open_read_only(options.journal, &identity)?;
    let journal_path = options.journal;
    if let Some(batch) = journal.outstanding_removal_batch()? {
        // A session's removal half settles with its insert half.
        if let Some(insert) = batch.with_insert_batch {
            let insert = journal.insert_batch(insert)?;
            drop(journal);
            return resume_insert_batch(journal_path, &identity, insert, &outer_control).await;
        }
        drop(journal);
        return resume_batch(journal_path, &identity, batch, &outer_control).await;
    }
    if let Some(batch) = journal.outstanding_insert_batch()? {
        drop(journal);
        return resume_insert_batch(journal_path, &identity, batch, &outer_control).await;
    }
    if let Some(batch) = journal.outstanding_metadata_batch()? {
        drop(journal);
        return resume_metadata_batch(journal_path, &identity, batch, &outer_control).await;
    }
    // Every other route judges the phone against the snapshot too: each row
    // must hold its audio, as in the capture of the run that started it.
    anyhow::ensure!(
        saved.index.unfilled().is_empty() && saved.index.gone().is_empty(),
        "no batch is unfinished, and {} holds catalogue rows without audio; resume a single attempt with the snapshot of the run that started it",
        options.snapshot.display()
    );
    // An attempt outstanding while a coordination is Active is that
    // coordination's own: it began on a quiescent journal and has owned it
    // since. Settling or cancelling the attempt acts under its permit, as the
    // coordination's own run would; the coordination stays Active.
    let _permit = if journal.outstanding_attempt()?.is_some()
        && let Some(active) = journal.active_coordination()?
    {
        Some(Journal::open(journal_path, &identity)?.enter_coordination(active.id)?)
    } else {
        None
    };
    // A Started insert or cover is first observed fresh: what the phone holds
    // now, not the given snapshot, decides whether it was never applied, left
    // a row without audio, or goes to its own route under `trace/recovery`.
    let recovery_trace;
    let (journal, options) = if options.cancel_prepared.is_none()
        && let Some(attempt) = journal.outstanding()?
        && attempt.state == crate::engine::state::journal::AttemptState::ReconciliationRequired
    {
        drop(journal);
        match observed::observe(
            journal_path,
            &identity,
            &attempt,
            &saved,
            options.run_dir,
            options.trace,
            &outer_control,
        )
        .await?
        {
            observed::Observed::Settled(report) => return Ok(report),
            observed::Observed::Locationless => {
                return compensate_insert(
                    journal_path,
                    &identity,
                    &attempt,
                    &options.trace.join("compensation"),
                    &outer_control,
                )
                .await;
            }
            observed::Observed::Applied => {}
        }
        recovery_trace = options.trace.join("recovery");
        (
            Journal::open_read_only(journal_path, &identity)?,
            Options {
                trace: &recovery_trace,
                ..options
            },
        )
    } else {
        (journal, options)
    };
    outer_control.check()?;
    journal.ensure_identity(&identity)?;
    if options.cancel_prepared.is_some() && journal.outstanding()?.is_none() {
        anyhow::bail!("journal has no outstanding attempt to cancel");
    }
    Box::pin(continue_run(options, engine, &identity, journal)).await
}

/// An unfinished removal batch: a Prepared batch was never sent and is
/// cancelled; a Started one is observed and settled, never resent.
async fn resume_batch(
    journal_path: &Path,
    identity: &Identity,
    batch: crate::engine::state::journal::BatchRecord,
    control: &Control,
) -> Result<Report> {
    use crate::{engine::execution::removal_batch, engine::state::journal::BatchState};
    let mut journal = Journal::open(journal_path, identity)?;
    let id = batch.id.as_i64();
    let (state, evidence, removed, deleted_playlists) = match batch.state {
        BatchState::Prepared => {
            journal.cancel_removal_batch(batch.id)?;
            (BatchState::Cancelled, batch.evidence.clone(), 0, 0)
        }
        BatchState::Started => {
            let settled = removal_batch::settle(
                &mut journal,
                identity,
                &batch,
                crate::engine::recovery::retained::playback::Window::LaterRun,
                control,
            )
            .await?;
            (
                settled.state,
                settled.evidence,
                settled.removed,
                settled.deleted_playlists,
            )
        }
        other => anyhow::bail!(
            "removal batch {id} is {} and needs no recovery",
            other.as_str()
        ),
    };
    Ok(Report {
        kind: Kind::Batch,
        successful: true,
        document: serde_json::json!({
            "schema":"rustunes.removal-batch-resume","batch":id,"kind":"removal",
            "status":state.as_str(),"removed":removed,"deleted_playlists":deleted_playlists,
            "evidence":evidence,"native_operation_replayed":false,
        }),
    })
}

/// The host name a compensating removal session announces.
const HOST_NAME: &str = "rustunes";

/// An unfinished insert batch, with its removal half when it is a session: a
/// Prepared batch was never sent and is cancelled, its conversions removed; a
/// Started one is observed and settled, never resent. A Partial batch (settled
/// now or earlier) has its rows without audio removed by one compensating
/// removal batch; the tracks it did bind stay.
async fn resume_insert_batch(
    journal_path: &Path,
    identity: &Identity,
    batch: crate::engine::state::journal::InsertBatchRecord,
    control: &Control,
) -> Result<Report> {
    use crate::{engine::execution::insert_batch, engine::state::journal::InsertBatchState};
    let id = batch.id.as_i64();
    let resumed = insert_batch::resume(journal_path, identity, &batch, HOST_NAME, control).await?;
    let compensation = match &resumed.compensation {
        Some(removal) => serde_json::json!({
            "removal_batch":removal.batch.as_i64(),"status":removal.state.as_str(),
            "removed":removal.removed,"evidence":removal.evidence,
        }),
        None => serde_json::Value::Null,
    };
    let state = resumed.state;
    let mut document = serde_json::json!({
        "schema":"rustunes.insert-batch-resume","batch":id,"kind":"insert",
        "status":state.as_str(),"inserted":resumed.inserted,"evidence":resumed.evidence,
        "compensation":compensation,"native_operation_replayed":false,
    });
    if let Some(removal) =
        Journal::open_read_only(journal_path, identity)?.session_removal(batch.id)?
    {
        document["session_removal"] = serde_json::json!({
            "batch":removal.id.as_i64(),"status":removal.state.as_str(),
            "removed":resumed.removed,
        });
    }
    Ok(Report {
        kind: Kind::Batch,
        successful: matches!(
            state,
            InsertBatchState::Verified
                | InsertBatchState::Cancelled
                | InsertBatchState::NotApplied
                | InsertBatchState::Compensated
        ),
        document,
    })
}

/// A Started Insert whose row a fresh observation shows without a file
/// location: the phone took the operation but never the audio. One
/// compensating removal batch takes the row out (evidence under `directory`),
/// and the attempt becomes Compensated. Nothing is resent.
async fn compensate_insert(
    journal_path: &Path,
    identity: &Identity,
    attempt: &crate::engine::state::journal::Attempt,
    directory: &Path,
    control: &Control,
) -> Result<Report> {
    use crate::engine::execution::removal_batch;
    let coordination = Journal::open_read_only(journal_path, identity)?
        .active_coordination()?
        .map(|c| c.id);
    let content = &attempt.prepared.content;
    let removal = removal_batch::compensate(
        journal_path,
        identity,
        directory,
        &[content.track_pid],
        std::slice::from_ref(&content.path),
        removal_batch::Purpose::CompensateInsert(attempt.id),
        coordination,
        HOST_NAME,
        control,
    )
    .await?;
    let state = Journal::open_read_only(journal_path, identity)?
        .attempt(attempt.id)?
        .state;
    Ok(Report {
        kind: Kind::Batch,
        successful: state == crate::engine::state::journal::AttemptState::Compensated,
        document: serde_json::json!({
            "schema":"rustunes.insert-compensation","kind":"insert",
            "attempt":attempt.id.as_i64(),"status":format!("{state:?}").to_lowercase(),
            "track":format!("{:016x}",content.track_pid),
            "compensation":{
                "removal_batch":removal.batch.as_i64(),"status":removal.state.as_str(),
                "removed":removal.removed,"evidence":removal.evidence,
            },
            "native_operation_replayed":false,
        }),
    })
}

/// An unfinished metadata batch: Prepared is cancelled, Started is observed.
async fn resume_metadata_batch(
    journal_path: &Path,
    identity: &Identity,
    batch: crate::engine::state::journal::MetadataBatchRecord,
    control: &Control,
) -> Result<Report> {
    use crate::{engine::execution::metadata_batch, engine::state::journal::MetadataBatchState};
    let mut journal = Journal::open(journal_path, identity)?;
    let id = batch.id.as_i64();
    let (state, evidence, updated) = match batch.state {
        MetadataBatchState::Prepared => {
            journal.cancel_metadata_batch(batch.id)?;
            (MetadataBatchState::Cancelled, batch.evidence.clone(), 0)
        }
        MetadataBatchState::Started => {
            let settled = metadata_batch::settle(
                &mut journal,
                identity,
                &batch,
                crate::engine::recovery::retained::playback::Window::LaterRun,
                control,
            )
            .await?;
            (settled.state, settled.evidence, settled.updated)
        }
        other => anyhow::bail!("metadata batch {id} is {other:?} and needs no recovery"),
    };
    Ok(Report {
        kind: Kind::Batch,
        successful: true,
        document: serde_json::json!({
            "schema":"rustunes.metadata-batch-resume","batch":id,"kind":"metadata",
            "status":state.as_str(),"updated":updated,"evidence":evidence,
            "native_operation_replayed":false,
        }),
    })
}

async fn continue_run(
    options: Options<'_>,
    engine: &crate::engine::Engine,
    identity: &Identity,
    journal: Journal,
) -> Result<Report> {
    let state_path = options.trace.join("state");
    let report = if let Some(id) = options.cancel_prepared {
        drop(journal);
        let mut journal = Journal::open(options.journal, identity)?;
        retained::cancel_prepared(&mut journal, identity, id, options.trace)?
    } else {
        Box::pin(run_ordinary(
            &options,
            &journal,
            &identity,
            engine,
            |deadline, cancelled| {
                current::capture_with_artwork_controlled(
                    &state_path,
                    &identity,
                    deadline,
                    cancelled,
                )
            },
            || Journal::open(options.journal, &identity),
        ))
        .await?
    };
    let successful = report.status != "uncertain";
    Ok(Report {
        kind: Kind::Ordinary,
        document: serde_json::to_value(report)?,
        successful,
    })
}

// Extract the ordinary writer-opening boundary so its ordering can be tested.
pub(crate) async fn run_ordinary<F, Fut, W>(
    options: &Options<'_>,
    read_only: &Journal,
    identity: &Identity,
    engine: &crate::engine::Engine,
    capture: F,
    open_writer: W,
) -> Result<retained::Report>
where
    F: FnOnce(Instant, Arc<AtomicBool>) -> Fut,
    Fut: Future<Output = Result<current::Current>>,
    W: FnOnce() -> Result<Journal>,
{
    read_only.ensure_identity(identity)?;
    if let retained::Dispatch::Local(report) = retained::dispatch(read_only)? {
        return Ok(report);
    }
    let run_dir = options
        .run_dir
        .context("resume requires the retained --run-dir")?;
    let control = engine.control(180);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(options.trace)
        .context("recovery trace directory must be new")?;
    engine.emit(crate::engine::Event::ReadingCurrentState)?;
    let current = timeout_at(
        control.deadline,
        capture(control.deadline, Arc::clone(&control.cancelled)),
    )
    .await
    .context("recovery state capture deadline")??;
    control.check()?;
    let checked = retained::check(
        read_only,
        &current,
        retained::Options {
            run_dir,
            trace: &options.trace.join("reconciliation"),
            current_state_dir: &options.trace.join("state"),
        },
        &control,
        device::provider,
    )
    .await?;
    if !checked.needs_writer() {
        return checked.into_report();
    }
    // The checked completion belongs to the original unchanged read-only
    // journal. Refusal and collection failures have never opened its writer.
    read_only.recheck_read_only_source()?;
    control.check()?;
    let mut writer = open_writer()?;
    checked.finish(&mut writer)
}

/// Stable recovery family and original evidence document returned to a frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ordinary,
    /// A removal, insert or metadata batch: settled by observation, never resent.
    Batch,
}
pub struct Report {
    pub kind: Kind,
    pub document: serde_json::Value,
    pub successful: bool,
}
mod observed;

#[cfg(test)]
mod tests;
