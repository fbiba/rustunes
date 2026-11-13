//! Settling what an earlier run left unfinished before a run that writes goes
//! on, as `rustunes resume` would: a batch from the journal, which names its
//! evidence, and a single insert or cover from the run that started it. The
//! phone is observed and nothing is sent again, so running an interrupted
//! `sync`, `push` or `delete` again carries on. A dry run changes nothing: it
//! names what settles the work instead (`Earlier::refuse`).
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};

use crate::engine::{
    Engine, Event,
    execution::replace::{
        ensure_no_unfinished_attempt, quoted, resume_command, run_holding, session_or_removal,
    },
    execution::{insert_batch, metadata_batch, removal_batch},
    recovery::{resume, retained, retained::playback::Window},
    state::journal::{AttemptState, BatchState, Identity, Journal, MetadataBatchState},
};

/// Where an earlier run's unfinished work is found and settled.
pub(crate) struct Earlier<'a> {
    pub journal: &'a Path,
    pub identity: &'a Identity,
    /// The state directory: its runs hold a single attempt's input.
    pub state: &'a Path,
    /// This run's capture, which identifies the phone.
    pub snapshot: &'a Path,
}

impl Earlier<'_> {
    /// Rows without audio (`unfilled`) may only be targets of the unfinished
    /// insertion that left them: an insert batch, or a single insert.
    pub(crate) fn only_unfinished(&self, unfilled: &BTreeSet<u64>) -> Result<()> {
        if unfilled.is_empty() {
            return Ok(());
        }
        let mut targets = BTreeSet::new();
        if self.journal.try_exists()? {
            let journal = Journal::open_read_only(self.journal, self.identity)?;
            if let Some(batch) = journal.outstanding_insert_batch()? {
                targets.extend(batch.targets.iter().map(|t| t.target.content.track_pid));
            }
            if let Some(attempt) = journal.outstanding_attempt()? {
                targets.insert(attempt.prepared.content.track_pid);
            }
        }
        if let Some(track) = unfilled.iter().find(|t| !targets.contains(t)) {
            bail!(
                "track {track:016x} has no audio on the phone, and no unfinished insertion of it was cut short; nothing was sent"
            );
        }
        Ok(())
    }

    /// A dry run changes nothing: refuse unfinished work, naming both ways
    /// to settle it. A coordination's single attempt is left to its command.
    pub(crate) fn refuse(&self, connection: &str) -> Result<()> {
        if !self.journal.try_exists()? {
            return Ok(());
        }
        let journal = Journal::open_read_only(self.journal, self.identity)?;
        let (unfinished, resume) = if let Some(batch) = unfinished_batch(&journal)? {
            (
                batch,
                format!(
                    "rustunes resume --snapshot {} --journal {} --trace-dir NEW_DIRECTORY{connection}",
                    quoted(&self.snapshot.to_string_lossy()),
                    quoted(&self.journal.to_string_lossy()),
                ),
            )
        } else if journal.active_coordination()?.is_none()
            && let Some(attempt) = journal.outstanding_attempt()?
        {
            let id = attempt.id.as_i64();
            let resume = match attempt.state {
                AttemptState::Prepared => format!(
                    "rustunes resume --snapshot {} --journal {} --trace-dir NEW_DIRECTORY --cancel-prepared {id}{connection}",
                    quoted(&self.snapshot.to_string_lossy()),
                    quoted(&self.journal.to_string_lossy()),
                ),
                _ => resume_command(self.journal, self.state, &attempt, connection),
            };
            (format!("attempt {id}"), resume)
        } else {
            return Ok(());
        };
        bail!(
            "{unfinished} from an earlier run is unfinished; the same command without --dry-run settles it first, or {resume} settles it alone; either observes the phone and never resends"
        )
    }

    /// Settle every unfinished batch (a session through its insert half),
    /// then, unless a coordination owns the journal, a single insert or
    /// cover, whose recovery trace goes in the new directory `trace`. None
    /// when nothing was unfinished; else the tracks a removal batch was
    /// verified to remove. The caller's capture is older than the settlement:
    /// it captures the phone again before going on.
    pub(crate) async fn settle(
        &self,
        trace: &Path,
        host_name: &str,
        engine: &Engine,
        seconds: u64,
    ) -> Result<Option<BTreeSet<u64>>> {
        if !self.journal.try_exists()? {
            return Ok(None);
        }
        let batch = self.settle_batch(host_name, engine, seconds).await?;
        let attempt = self.settle_attempt(trace, engine).await?;
        Ok(match (batch, attempt) {
            (Some(removed), _) => Some(removed),
            (None, true) => Some(BTreeSet::new()),
            (None, false) => None,
        })
    }

    async fn settle_batch(
        &self,
        host_name: &str,
        engine: &Engine,
        seconds: u64,
    ) -> Result<Option<BTreeSet<u64>>> {
        let (journal_path, identity) = (self.journal, self.identity);
        let journal = Journal::open_read_only(journal_path, identity)?;
        let Some(unfinished) = unfinished_batch(&journal)? else {
            return Ok(None);
        };
        let not_settled =
            || format!("{unfinished} from an earlier run is not settled; nothing was sent");
        let control = engine.control(seconds);
        let removed = if let Some(batch) = journal.outstanding_removal_batch()? {
            let id = batch.id.as_i64();
            let state = match batch.with_insert_batch {
                // A session settles through its insert half.
                Some(insert) => {
                    let record = journal.insert_batch(insert)?;
                    drop(journal);
                    insert_batch::resume(journal_path, identity, &record, host_name, &control)
                        .await
                        .with_context(not_settled)?;
                    Journal::open_read_only(journal_path, identity)?
                        .removal_batch(batch.id)?
                        .state
                }
                None => {
                    drop(journal);
                    let mut journal = Journal::open(journal_path, identity)?;
                    match batch.state {
                        BatchState::Prepared => {
                            journal.cancel_removal_batch(batch.id)?;
                            BatchState::Cancelled
                        }
                        // A batch of an earlier run: the phone may have been
                        // played since.
                        _ => {
                            removal_batch::settle(
                                &mut journal,
                                identity,
                                &batch,
                                Window::LaterRun,
                                &control,
                            )
                            .await
                            .with_context(not_settled)?
                            .state
                        }
                    }
                }
            };
            settled(engine, "removal", id, state.as_str())?;
            match state {
                BatchState::Verified => batch.targets.iter().map(|t| t.track).collect(),
                _ => BTreeSet::new(),
            }
        } else if let Some(batch) = journal.outstanding_insert_batch()? {
            drop(journal);
            // A Partial batch has the rows it left without audio removed by
            // one compensating removal batch; the tracks it bound stay.
            let resumed = insert_batch::resume(journal_path, identity, &batch, host_name, &control)
                .await
                .with_context(not_settled)?;
            settled(engine, "insert", batch.id.as_i64(), resumed.state.as_str())?;
            BTreeSet::new()
        } else if let Some(batch) = journal.outstanding_metadata_batch()? {
            drop(journal);
            let mut journal = Journal::open(journal_path, identity)?;
            let state = match batch.state {
                MetadataBatchState::Prepared => {
                    journal.cancel_metadata_batch(batch.id)?;
                    MetadataBatchState::Cancelled
                }
                _ => {
                    metadata_batch::settle(
                        &mut journal,
                        identity,
                        &batch,
                        Window::LaterRun,
                        &control,
                    )
                    .await
                    .with_context(not_settled)?
                    .state
                }
            };
            settled(engine, "metadata", batch.id.as_i64(), state.as_str())?;
            BTreeSet::new()
        } else {
            unreachable!("an unfinished batch is one of the three kinds")
        };
        // Settling must leave nothing outstanding, or the run would settle
        // the same batch again.
        let left = unfinished_batch(&Journal::open_read_only(journal_path, identity)?)?;
        ensure!(
            left.is_none(),
            "{unfinished} from an earlier run is still unfinished after settling; nothing was sent"
        );
        Ok(Some(removed))
    }

    /// An earlier run's single insert or cover: a Prepared one sent nothing
    /// and is cancelled; a Started one is observed with the snapshot and
    /// input of the run that started it, found under the state directory,
    /// as `resume --run-dir` does. Without that run it refuses, naming the
    /// `resume` that settles it.
    async fn settle_attempt(&self, trace: &Path, engine: &Engine) -> Result<bool> {
        let (journal_path, identity) = (self.journal, self.identity);
        let journal = Journal::open_read_only(journal_path, identity)?;
        // A coordination's own attempt is settled by its command (`replace`).
        if journal.active_coordination()?.is_some() {
            return Ok(false);
        }
        let Some(attempt) = journal.outstanding_attempt()? else {
            return Ok(false);
        };
        drop(journal);
        let id = attempt.id.as_i64();
        let status = if attempt.state == AttemptState::Prepared {
            let mut journal = Journal::open(journal_path, identity)?;
            retained::cancel_prepared(&mut journal, identity, id, trace)?
                .status
                .to_owned()
        } else {
            let Some(run) = run_holding(
                &self.state.join("runs"),
                attempt.prepared.input_receipt_sha256,
            ) else {
                ensure_no_unfinished_attempt(
                    journal_path,
                    identity,
                    self.state,
                    self.snapshot,
                    &engine.command_options(),
                )?;
                bail!("attempt {id} from an earlier run is unfinished");
            };
            let snapshot: PathBuf = run.join("snapshot");
            let report = Box::pin(resume::run(
                resume::Options {
                    snapshot: &snapshot,
                    journal: journal_path,
                    run_dir: Some(&run),
                    trace,
                    cancel_prepared: None,
                    selected_device: engine.selected_device(),
                },
                engine,
            ))
            .await
            .with_context(|| {
                format!(
                    "attempt {id} from an earlier run is not settled; nothing was sent; its trace: {}",
                    trace.display()
                )
            })?;
            let status = report.document["status"]
                .as_str()
                .unwrap_or("settled")
                .to_owned();
            ensure!(
                report.successful,
                "attempt {id} from an earlier run ended {status}; nothing was sent; its trace: {}",
                trace.display()
            );
            status
        };
        ensure!(
            Journal::open_read_only(journal_path, identity)?
                .outstanding_attempt()?
                .is_none(),
            "attempt {id} from an earlier run is still unfinished after settling; nothing was sent"
        );
        engine.emit(Event::EarlierSettled {
            kind: "attempt",
            id,
            state: &status,
        })?;
        Ok(true)
    }
}

/// The journal's unfinished batch by name, a session's by both halves.
fn unfinished_batch(journal: &Journal) -> Result<Option<String>> {
    Ok(if let Some(b) = journal.outstanding_removal_batch()? {
        Some(session_or_removal(&b))
    } else if let Some(b) = journal.outstanding_insert_batch()? {
        Some(format!("insert batch {}", b.id.as_i64()))
    } else {
        journal
            .outstanding_metadata_batch()?
            .map(|b| format!("metadata batch {}", b.id.as_i64()))
    })
}

fn settled(engine: &Engine, kind: &'static str, id: i64, state: &str) -> Result<()> {
    engine.emit(Event::EarlierSettled { kind, id, state })
}

#[cfg(test)]
mod tests;
