//! `plan --remove-missing`, `--add-remove`, `--replace-changed` and
//! `--replace-library`: the saved plan, shown in full.
use crate::engine::{self, ChangePlan};
use anyhow::Result;
use std::io::{IsTerminal, Write};

pub(super) fn plan(request: engine::ChangePlanRequest<'_>, json: bool) -> Result<bool> {
    let engine = super::configured_engine()?;
    let planned = engine.plan_change(request)?;
    if super::machine::enabled() {
        if !planned.plan.ready() {
            super::machine::outcome("blocked");
        }
        super::machine::report_bytes("change_plan", &planned.serialized)?;
        return Ok(planned.plan.ready());
    }
    let mut out = std::io::stdout().lock();
    if json {
        out.write_all(&planned.serialized)?;
    } else {
        show(&planned.plan, &mut out)?;
        writeln!(
            out,
            "Saved snapshot revision {}; apply checks the phone's current state against this plan before each step.",
            planned.plan.media_anchor
        )?;
    }
    out.flush()?;
    Ok(planned.plan.ready())
}

fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}

/// Every action, cost and blocker of `plan`.
pub(super) fn show(plan: &ChangePlan, out: &mut impl Write) -> Result<()> {
    let flag = match plan.action {
        "remove_missing" => "--remove-missing",
        "add_remove" => "--add-remove",
        "replace_changed" => "--replace-changed",
        _ => "--replace-library",
    };
    writeln!(
        out,
        "Change plan (sync {flag}): remove {} track(s), then add {} and update {}; {} track(s) untouched.",
        plan.removals.len(),
        plan.additions.len(),
        plan.updates.len(),
        plan.untouched_tracks
    )?;
    for removal in &plan.removals {
        let cause = match (removal.cause, &removal.missing_source_path) {
            ("source_gone", Some(path)) => format!("source gone ({path})"),
            ("audio_changed", _) => "audio changed".into(),
            ("field_cleared", _) => "its source cleared a field an edit cannot clear".into(),
            ("whole_library", _) => "whole library".into(),
            (cause, _) => cause.into(),
        };
        let replaced_by = removal
            .replaced_by
            .as_deref()
            .map(|s| format!(" → {s:?}"))
            .unwrap_or_default();
        writeln!(
            out,
            "REMOVE {} {:?} — {:?} ({:?}; {} plays, {} skips): {cause}{replaced_by}",
            removal.track_id,
            removal.title,
            removal.artist,
            removal.album,
            removal.plays,
            removal.skips
        )?;
        for blocker in &removal.blockers {
            writeln!(out, "  BLOCKED: {blocker}")?;
        }
    }
    for addition in &plan.additions {
        let mut facts = vec![addition.codec.to_ascii_uppercase()];
        if let Some(bytes) = addition.file_bytes {
            facts.push(size(bytes));
        }
        if addition.has_cover {
            facts.push("cover".into());
        }
        let replacing = addition
            .replaces
            .as_deref()
            .map(|id| format!(" replacing {id}"))
            .unwrap_or_default();
        writeln!(
            out,
            "ADD {:?} ({}){replacing}",
            addition.relative_path,
            facts.join(", ")
        )?;
        if let Some(why) = &addition.cover_left_out {
            writeln!(out, "  {why}")?;
        }
    }
    for update in &plan.updates {
        writeln!(
            out,
            "UPDATE {:?} → {}",
            update.relative_path,
            update.decision["track_id"].as_str().unwrap_or("?")
        )?;
        for change in update.decision["changes"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "  {}: {} → {}",
                change["field"].as_str().unwrap_or("?"),
                change["current"],
                change["desired"]
            )?;
        }
    }
    for skipped in &plan.skipped {
        writeln!(out, "SKIP {:?}: {}", skipped.relative_path, skipped.reason)?;
    }
    for cover in &plan.pending_covers {
        writeln!(out, "FINISH COVER {cover:?}")?;
    }
    if !plan.source_path_unknown.is_empty() {
        writeln!(
            out,
            "KEEP {} track(s) this tool inserted whose source path the journal does not pin.",
            plan.source_path_unknown.len()
        )?;
    }
    let costs = &plan.costs;
    writeln!(
        out,
        "{} removal session(s) of up to {} tracks; {} to upload; {} plays and {} skips lost.",
        costs.removal_sessions,
        costs.removal_batch_size,
        size(costs.upload_bytes),
        costs.plays_lost,
        costs.skips_lost
    )?;
    if !plan.removals.is_empty() {
        writeln!(out, "{}.", capitalized(plan.losses))?;
    }
    writeln!(out, "{}.", capitalized(plan.dependencies))?;
    for blocker in &plan.blockers {
        writeln!(out, "BLOCKED: {blocker}")?;
    }
    Ok(())
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Show the plan and ask before removing anything, unless `yes`.
pub(super) fn confirm(plan: &ChangePlan, yes: bool) -> Result<bool> {
    if super::machine::enabled() {
        super::machine::report("change_plan", plan)?;
    } else {
        show(plan, &mut std::io::stdout().lock())?;
    }
    if yes {
        return Ok(true);
    }
    let prompt = format!(
        "Remove {} track(s) from the iPhone, then sync as planned?",
        plan.removals.len()
    );
    if super::machine::enabled() {
        return super::machine::confirm(
            "apply_change_plan",
            prompt,
            serde_json::json!({
                "removals": plan.removals.len(),
                "additions": plan.additions.len(),
                "updates": plan.updates.len(),
            }),
        );
    }
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "a noninteractive apply that removes tracks requires --yes; use --dry-run to check it"
    );
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(super) fn outcome(outcome: engine::ChangeApplyOutcome) -> Result<()> {
    use engine::ChangeApplyOutcome as Outcome;
    let line = outcome_line(outcome);
    if !super::machine::enabled() {
        println!("{line}");
        return Ok(());
    }
    let (name, mut report) = match outcome {
        Outcome::Checked {
            removals,
            additions,
            updates,
        } => (
            "checked",
            serde_json::json!({"removals": removals, "additions": additions, "updates": updates}),
        ),
        Outcome::Preflight {
            removals,
            additions,
            updates,
            sync_checked,
        } => (
            "dry_run",
            serde_json::json!({
                "removals": removals,
                "additions": additions,
                "updates": updates,
                "sync_checked": sync_checked,
            }),
        ),
        Outcome::Cancelled => ("cancelled", serde_json::json!({})),
        Outcome::Completed {
            removed,
            additions,
            updates,
        } => (
            "completed",
            serde_json::json!({"removed": removed, "additions": additions, "updates": updates}),
        ),
    };
    report["outcome"] = name.into();
    report["message"] = line.into();
    super::machine::outcome(name);
    super::machine::report("change_apply", &report)
}

fn outcome_line(outcome: engine::ChangeApplyOutcome) -> String {
    use engine::ChangeApplyOutcome as Outcome;
    // An additive plan removes nothing, so its lines leave removals out.
    let removing = |removals: usize| match removals {
        0 => String::new(),
        n => format!("{n} removals, "),
    };
    match outcome {
        Outcome::Checked {
            removals,
            additions,
            updates,
        } => format!(
            "Saved plan rebuilt exactly: {}{additions} additions, {updates} updates; no live device check or phone writes.",
            removing(removals)
        ),
        Outcome::Preflight {
            removals,
            additions,
            updates,
            sync_checked: true,
        } => format!(
            "Live preflight passed: the phone's current state gives the same {}{additions} additions and {updates} updates; nothing was sent.",
            removing(removals)
        ),
        Outcome::Preflight {
            removals,
            additions,
            sync_checked: false,
            ..
        } => format!(
            "Live preflight passed: the phone's current state gives the same {removals} removals; the {additions} additions are planned once they are gone and checked then, before any is sent; nothing was sent."
        ),
        Outcome::Cancelled => format!("Cancelled; nothing was sent."),
        Outcome::Completed {
            removed: 0,
            additions,
            updates,
        } => format!("Plan applied: the sync added {additions} and updated {updates} as planned."),
        Outcome::Completed {
            removed,
            additions,
            updates,
        } => format!(
            "Plan applied: {removed} track(s) removed in this run, then the sync added {additions} and updated {updates} as planned."
        ),
    }
}
