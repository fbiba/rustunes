//! `apply` for a saved change plan (`planning::change`): rebuild the plan
//! exactly from its snapshot, sources and journal, then run the same change
//! `sync` runs from fresh captures, each step pinned to the plan. The removal
//! (verified removal batches, or a replacement coordination) must select
//! exactly the plan's tracks, and the sync after it must add and update
//! exactly the plan's sources; a step that differs refuses before it sends.
//! A saved additive plan (`plan` without a removal) runs that sync alone.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::{
    engine::planning::saved_plan,
    engine::planning::source_collection::SelectionOptions,
    engine::state::current::hex,
    engine::{
        ChangePlan, Engine,
        execution::{delete, replace, sync},
        planning::change::{self, Action, MAX_BATCH, Pinned},
    },
};
use music_protocol::protocol::trace::private_write;

pub struct Request<'a> {
    pub plan: &'a Path,
    pub snapshot: &'a Path,
    pub journal: &'a Path,
    /// New private directory for this run's receipts.
    pub trace: &'a Path,
    /// Run history of the steps (`runs/`).
    pub state: &'a Path,
    pub host_name: &'a str,
    pub seconds: u64,
    /// Operations per session, besides `update_db_info`, in the removal and
    /// in the sync after it.
    pub batch_size: usize,
    /// Rebuild the plan offline only.
    pub check_only: bool,
    /// Check every step against fresh state and send nothing.
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Checked {
        removals: usize,
        additions: usize,
        updates: usize,
    },
    Preflight {
        removals: usize,
        additions: usize,
        updates: usize,
        /// A replacement's sync is planned only once its removal is done, so
        /// its dry run checks the removal alone.
        sync_checked: bool,
    },
    Cancelled,
    Completed {
        removed: usize,
        additions: usize,
        updates: usize,
    },
}

pub(crate) async fn run(
    r: Request<'_>,
    engine: &Engine,
    confirm: &mut impl FnMut(&ChangePlan) -> Result<bool>,
) -> Result<Outcome> {
    engine.check()?;
    ensure!(
        (1..=MAX_BATCH).contains(&r.batch_size),
        "batch size must be 1..{MAX_BATCH}"
    );
    if !r.check_only {
        ensure!(!r.trace.try_exists()?, "apply trace directory must be new");
    }
    if !change::is_change_plan(r.plan)? {
        return Box::pin(additive(r, engine)).await;
    }
    let loaded = {
        let engine = engine.clone();
        let (plan, snapshot, journal) = (
            r.plan.to_path_buf(),
            r.snapshot.to_path_buf(),
            r.journal.to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            engine.with_observer(|| change::load(&plan, &snapshot, &journal, &engine))
        })
        .await
        .context("saved plan task failed")??
    };
    let plan = &loaded.plan;
    ensure!(
        plan.ready(),
        "the saved plan is blocked; nothing was sent:\n  {}",
        plan.blockers.join("\n  ")
    );
    let (removals, additions, updates) = (
        plan.removals.len(),
        plan.additions.len(),
        plan.updates.len(),
    );
    if r.check_only {
        return Ok(Outcome::Checked {
            removals,
            additions,
            updates,
        });
    }
    crate::engine::execution::native_insert::create_trace(r.trace)?;
    private_write(
        &r.trace.join("apply.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.change-apply","plan":std::path::absolute(r.plan)?,
            "plan_sha256":hex(&loaded.plan_sha256),"action":plan.action,"dry_run":r.dry_run,
            "removals":removals,"additions":additions,"updates":updates,
            "batch_size":r.batch_size,
        }))?,
    )?;
    if !r.dry_run && removals > 0 && !confirm(plan)? {
        return Ok(Outcome::Cancelled);
    }
    let pinned = &loaded.pinned;
    let removed = match loaded.action {
        Action::ReplaceChanged | Action::ReplaceLibrary => {
            let outcome = replace::run(
                replace::Request {
                    kind: if loaded.action == Action::ReplaceLibrary {
                        replace::Kind::Library
                    } else {
                        replace::Kind::Changed
                    },
                    source: &loaded.source,
                    state: r.state,
                    journal: Some(r.journal),
                    selection: &loaded.selection,
                    host_name: r.host_name,
                    seconds: r.seconds,
                    batch_size: r.batch_size,
                    dry_run: r.dry_run,
                    flac_to_alac: loaded.flac_to_alac,
                    pinned: Some(pinned),
                },
                engine,
                &mut |_| Ok(true),
            )
            .await?;
            match outcome {
                replace::Outcome::Completed { removed, .. } => removed,
                replace::Outcome::Nothing | replace::Outcome::Preview => 0,
                replace::Outcome::Cancelled => return Ok(Outcome::Cancelled),
            }
        }
        Action::RemoveMissing | Action::AddRemove => {
            let outcome = delete::run(
                delete::Options {
                    dry_run: r.dry_run,
                    snapshot: None,
                    journal: Some(r.journal.to_path_buf()),
                    state_dir: r.state.to_path_buf(),
                    output: None,
                    host_name: r.host_name.to_owned(),
                    seconds: r.seconds,
                    batch_size: r.batch_size,
                    leave_for_sync: true,
                    pinned: Some(pinned.clone()),
                },
                delete::Mode::Missing {
                    source: loaded.source.clone(),
                    selection: loaded.selection.clone(),
                },
                engine.selected_device(),
                engine,
                &mut |_| Ok(true),
            )
            .await?;
            let (removed, left) = match outcome {
                delete::Outcome::Completed { removed, left, .. } => (removed, left),
                delete::Outcome::Preview => (0, Vec::new()),
                delete::Outcome::Cancelled => return Ok(Outcome::Cancelled),
                delete::Outcome::Blocked | delete::Outcome::Held { .. } => {
                    anyhow::bail!(
                        "the removal is blocked against the phone's current state; nothing was sent"
                    )
                }
            };
            let synced = Box::pin(sync::run(
                sync::Options {
                    source: &loaded.source,
                    state: r.state,
                    snapshot: None,
                    journal: Some(r.journal),
                    selection: &loaded.selection,
                    host_name: r.host_name,
                    dry_run: r.dry_run,
                    seconds: r.seconds,
                    add_only: loaded.action.add_only(),
                    batch_size: r.batch_size,
                    flac_to_alac: loaded.flac_to_alac,
                    pinned: Some(pinned),
                    library: None,
                    removals: &left,
                },
                engine,
            ))
            .await
            .with_context(|| match removed {
                0 => "nothing was removed".to_owned(),
                n => format!("the removal finished ({n} track(s)); plan again from a fresh snapshot for the rest"),
            })?;
            let removed = removed + synced.removed;
            ensure!(
                synced.ready(),
                "the sync is blocked ({}); {removed} track(s) were removed",
                synced.plan.display()
            );
            removed
        }
    };
    let outcome = if r.dry_run {
        Outcome::Preflight {
            removals,
            additions,
            updates,
            sync_checked: !loaded.action.replaces(),
        }
    } else {
        Outcome::Completed {
            removed,
            additions,
            updates,
        }
    };
    private_write(
        &r.trace.join("outcome.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.change-apply-outcome","dry_run":r.dry_run,
            "removed":removed,"planned_removals":removals,"planned_additions":additions,
            "planned_updates":updates,
        }))?,
    )?;
    Ok(outcome)
}

/// A saved additive plan rebuilt exactly, with what its sync is held to.
struct Additive {
    source: PathBuf,
    selection: SelectionOptions,
    add_only: bool,
    flac_to_alac: bool,
    plan_sha256: [u8; 32],
    pinned: Pinned,
}

/// `apply` for a saved additive plan: rebuild it exactly, then run the sync
/// it describes from a fresh capture, pinned to its additions and updates,
/// with insert batches and covers as `sync` sends them.
async fn additive(r: Request<'_>, engine: &Engine) -> Result<Outcome> {
    let loaded = {
        let engine = engine.clone();
        let (plan, snapshot, journal) = (
            r.plan.to_path_buf(),
            r.snapshot.to_path_buf(),
            r.journal.to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            engine.with_observer(|| -> Result<Additive> {
                let validated = saved_plan::load_read_only(&plan, &snapshot, &journal)?;
                let plan = &validated.plan;
                Ok(Additive {
                    source: PathBuf::from(&plan.source_root),
                    selection: SelectionOptions {
                        recursive: plan.selection.recursive,
                        include: plan.selection.include.clone(),
                        exclude: plan.selection.exclude.clone(),
                    },
                    add_only: plan.preserve_existing_tracks,
                    flac_to_alac: plan.flac_to_alac,
                    plan_sha256: validated.plan_sha256,
                    pinned: Pinned::from_additive(plan)?,
                })
            })
        })
        .await
        .context("saved plan task failed")??
    };
    let (additions, updates) = (loaded.pinned.additions.len(), loaded.pinned.updates.len());
    if r.check_only {
        return Ok(Outcome::Checked {
            removals: 0,
            additions,
            updates,
        });
    }
    crate::engine::execution::native_insert::create_trace(r.trace)?;
    private_write(
        &r.trace.join("apply.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.change-apply","plan":std::path::absolute(r.plan)?,
            "plan_sha256":hex(&loaded.plan_sha256),"action":"additive","dry_run":r.dry_run,
            "removals":0,"additions":additions,"updates":updates,
            "batch_size":r.batch_size,
        }))?,
    )?;
    let synced = Box::pin(sync::run(
        sync::Options {
            source: &loaded.source,
            state: r.state,
            snapshot: None,
            journal: Some(r.journal),
            selection: &loaded.selection,
            host_name: r.host_name,
            dry_run: r.dry_run,
            seconds: r.seconds,
            add_only: loaded.add_only,
            batch_size: r.batch_size,
            flac_to_alac: loaded.flac_to_alac,
            pinned: Some(&loaded.pinned),
            library: None,
            removals: &[],
        },
        engine,
    ))
    .await?;
    ensure!(
        synced.ready(),
        "the sync is blocked ({}); nothing was sent",
        synced.plan.display()
    );
    let outcome = if r.dry_run {
        Outcome::Preflight {
            removals: 0,
            additions,
            updates,
            sync_checked: true,
        }
    } else {
        Outcome::Completed {
            removed: 0,
            additions,
            updates,
        }
    };
    private_write(
        &r.trace.join("outcome.json"),
        &serde_json::to_vec_pretty(&json!({
            "schema":"rustunes.change-apply-outcome","dry_run":r.dry_run,
            "removed":0,"planned_removals":0,"planned_additions":additions,
            "planned_updates":updates,
        }))?,
    )?;
    Ok(outcome)
}
