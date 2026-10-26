//! `plan --remove-missing`, `--add-remove`, `--replace-changed` and
//! `--replace-library`: the saved plan, shown in full.
use crate::engine::{self, ChangePlan};
use anyhow::Result;
use std::io::{IsTerminal, Write};

fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.1} MB", bytes as f64 / 1e6)
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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
