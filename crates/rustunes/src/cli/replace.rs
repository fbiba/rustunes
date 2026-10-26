//! `sync --replace-library` and `sync --replace-changed`: the proposal, an
//! explicit confirmation and the result.
use crate::engine::{self, ReplaceKind, ReplaceOutcome, ReplacePreview};
use anyhow::{Result, ensure};
use std::io::{IsTerminal, Write};

pub(super) async fn run(
    request: engine::ReplaceRequest<'_>,
    yes: bool,
    engine: &engine::Engine,
) -> Result<bool> {
    let dry_run = request.dry_run;
    let outcome = engine
        .replace(request, &mut |preview| {
            show(&preview)?;
            if dry_run {
                return Ok(false);
            }
            confirm(&preview, yes)
        })
        .await?;
    let line = match &outcome {
        ReplaceOutcome::Nothing => "No managed track has changed audio; nothing to replace.".into(),
        ReplaceOutcome::Preview => "Dry run; nothing was sent.".into(),
        ReplaceOutcome::Cancelled => "Cancelled; nothing was sent.".into(),
        ReplaceOutcome::Completed {
            coordination,
            removed,
            tracks,
        } => format!(
            "Replacement {coordination} complete: {removed} track(s) removed in this run; the phone holds {tracks} track(s) and every selected source is on it."
        ),
    };
    if super::machine::enabled() {
        let (name, mut report) = match &outcome {
            ReplaceOutcome::Nothing => ("nothing_to_do", serde_json::json!({})),
            ReplaceOutcome::Preview => ("dry_run", serde_json::json!({})),
            ReplaceOutcome::Cancelled => ("cancelled", serde_json::json!({})),
            ReplaceOutcome::Completed {
                coordination,
                removed,
                tracks,
            } => (
                "completed",
                serde_json::json!({
                    "coordination": coordination,
                    "removed": removed,
                    "tracks": tracks,
                }),
            ),
        };
        report["outcome"] = name.into();
        report["message"] = line.into();
        super::machine::outcome(name);
        super::machine::report("replacement", &report)?;
    } else {
        println!("{line}");
    }
    Ok(!matches!(outcome, ReplaceOutcome::Cancelled))
}

fn show(preview: &ReplacePreview<'_>) -> Result<()> {
    if super::machine::enabled() {
        return super::machine::report(
            "replacement_proposal",
            &serde_json::json!({
                "kind": match preview.kind {
                    ReplaceKind::Library => "library",
                    ReplaceKind::Changed => "changed",
                },
                "continuing": preview.continuing,
                "removals": preview.removals,
                "sources": preview.sources,
                "selection": preview.selection,
            }),
        );
    }
    if let Some(id) = preview.continuing {
        println!("Continuing replacement {id}.");
    }
    match preview.kind {
        ReplaceKind::Library => println!(
            "Replace the library: remove {} track(s) from the phone, then add {} source file(s).",
            preview.removals.len(),
            preview.sources
        ),
        ReplaceKind::Changed if preview.continuing.is_some() && preview.removals.is_empty() => {
            println!("Every track it replaces is off the phone; adding the sources again.")
        }
        ReplaceKind::Changed => println!(
            "Replace {} track(s) whose source audio changed or that an edit cannot bring up to date.",
            preview.removals.len()
        ),
    }
    for removal in preview.removals {
        let replaced_by = removal
            .source
            .as_deref()
            .map(|s| format!(" → {s}"))
            .unwrap_or_default();
        println!(
            "REMOVE {} \"{}\" — \"{}\" ({} plays){replaced_by}",
            removal.track, removal.title, removal.artist, removal.plays
        );
    }
    println!(
        "Removed tracks lose their identities, play and skip counts, ratings and playlist memberships."
    );
    println!("Proposal: {}", preview.selection.display());
    Ok(())
}

fn confirm(preview: &ReplacePreview<'_>, yes: bool) -> Result<bool> {
    if yes || (preview.continuing.is_some() && preview.removals.is_empty()) {
        return Ok(true);
    }
    let prompt = format!(
        "Remove {} track(s) from the iPhone and add the sources again?",
        preview.removals.len()
    );
    if super::machine::enabled() {
        return super::machine::confirm(
            "replace_tracks",
            prompt,
            serde_json::json!({"tracks": preview.removals.len(), "sources": preview.sources}),
        );
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "a noninteractive replacement requires --yes; use --dry-run to see it"
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
