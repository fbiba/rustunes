//! `rustunes init` and `rustunes device check`: show the phone's readiness,
//! the plan for its first session, and ask before anything is written.
use std::io::{IsTerminal, Write};

use anyhow::Result;
use serde_json::Value;

use crate::engine::{self, InitOutcome, InitPlan};

/// Show the plan and ask before writing anything, unless `yes`.
pub(super) fn confirm(plan: &InitPlan, yes: bool) -> Result<bool> {
    if super::machine::enabled() {
        super::machine::report("init_plan", &engine::init_plan_json(plan))?;
    } else {
        show_plan(plan, &mut std::io::stdout().lock())?;
    }
    if yes {
        return Ok(true);
    }
    let prompt = format!(
        "Prepare \"{}\" for rustunes? Its first session cannot be repeated.",
        plan.device_name
    );
    if super::machine::enabled() {
        return super::machine::confirm(
            "init",
            prompt,
            serde_json::json!({"device": plan.device_name}),
        );
    }
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "a noninteractive init requires --yes; use --dry-run to check it"
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

/// What to run once the phone is initialised. A first sync needs no
/// `rustunes pull`: it starts the journal of a phone init prepared that
/// holds no track.
const NEXT: &str = "Next, run rustunes sync --dry-run to see what the first sync sends, then \
                    rustunes sync (or rustunes push PATH to send chosen files).";

pub(super) fn outcome(outcome: &InitOutcome) -> Result<()> {
    let (line, plan) = match outcome {
        InitOutcome::Planned(plan) => (
            format!(
                "Dry run: nothing was written to the iPhone. Plan saved to {}",
                plan.run.join("plan.json").display()
            ),
            Some(plan),
        ),
        InitOutcome::AlreadyInitialised { .. } => {
            (format!("This iPhone is already initialised. {NEXT}"), None)
        }
        InitOutcome::Initialised { plan, playlist } => (
            format!(
                "Initialised \"{}\": it now holds rustunes' library.{} {NEXT}",
                plan.device_name,
                if *playlist {
                    ""
                } else {
                    " The iPhone did not keep the library playlist, as it does while iCloud Music Library is on."
                }
            ),
            Some(plan),
        ),
    };
    if super::machine::enabled() {
        let mut report = serde_json::json!({
            "outcome": match outcome {
                InitOutcome::Planned(_) => "planned",
                InitOutcome::AlreadyInitialised { .. } => "already_initialised",
                InitOutcome::Initialised { .. } => "initialised",
            },
            "message": line,
        });
        if let Some(plan) = plan {
            report["run"] = plan.run.display().to_string().into();
        }
        if let InitOutcome::Initialised { playlist, .. } = outcome {
            report["library_playlist_kept"] = (*playlist).into();
        }
        return super::machine::report("init", &report);
    }
    if let InitOutcome::Planned(plan) = outcome {
        show_plan(plan, &mut std::io::stdout().lock())?;
    }
    println!("{line}");
    Ok(())
}

fn show_plan(plan: &InitPlan, out: &mut impl Write) -> Result<()> {
    writeln!(out, "iPhone: \"{}\", {}", plan.device_name, plan.product)?;
    writeln!(
        out,
        "No computer has synced it{}. rustunes will:",
        if plan.retry {
            " (an earlier init stopped before its session finished)"
        } else {
            ""
        }
    )?;
    if !plan.remove.is_empty() {
        writeln!(
            out,
            "  - remove what the earlier attempt left part-written or staged, \
             once the iPhone has accepted the session: {}",
            plan.remove.join(", ")
        )?;
    }
    if !plan.create_folders.is_empty() {
        writeln!(out, "  - create {}", plan.create_folders.join(", "))?;
    }
    match plan.write_prefs.as_slice() {
        [] => {}
        [_, _] => writeln!(
            out,
            "  - write iTunesPrefs and iTunesPrefs.plist for a new library, kept in rustunes' state"
        )?,
        files => writeln!(
            out,
            "  - write {}, which the earlier attempt did not reach",
            files.join(", ")
        )?,
    }
    writeln!(
        out,
        "  - run one sync session (Delta, Media anchor 0 to 1) that sends only\n    \
         update_db_info (database {:016X}, library playlist {:016X}) and\n    \
         the library playlist itself: hidden, named \"{}\", no tracks",
        plan.database_pid, plan.library_playlist_pid, plan.device_name
    )?;
    writeln!(
        out,
        "No track is sent: add music afterwards with rustunes sync or rustunes push."
    )?;
    Ok(())
}

/// `rustunes device check`.
pub(super) fn check(report: &Value, json: bool) -> Result<()> {
    if super::machine::enabled() {
        return super::machine::report("device_check", report);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    show_check(report, &mut std::io::stdout().lock())
}

fn show_check(report: &Value, out: &mut impl Write) -> Result<()> {
    let device = &report["device"];
    writeln!(
        out,
        "iPhone: {}, {}, iOS {} ({})",
        device["name"],
        device["product_type"].as_str().unwrap_or("?"),
        device["product_version"].as_str().unwrap_or("?"),
        device["build_version"].as_str().unwrap_or("?")
    )?;
    for path in report["paths"].as_array().into_iter().flatten() {
        let state = match (path["present"].as_bool(), path["kind"].as_str()) {
            (Some(false), _) => "absent".to_owned(),
            (_, Some("file")) => format!("{} bytes", path["bytes"]),
            (_, Some(kind)) => kind.to_owned(),
            _ => "?".to_owned(),
        };
        writeln!(
            out,
            "  {:<50} {state}",
            path["path"].as_str().unwrap_or("?")
        )?;
    }
    if let Some(folders) = report["music_folders"].as_u64() {
        writeln!(out, "Music folders (Fnn): {folders}")?;
    }
    if let Some(staged) = report["staged"]
        .as_array()
        .filter(|names| !names.is_empty())
    {
        let names: Vec<_> = staged.iter().filter_map(Value::as_str).collect();
        writeln!(out, "In Sync/Media: {}", names.join(", "))?;
    }
    writeln!(
        out,
        "Library identity: {}",
        report["library_identity"]["state"].as_str().unwrap_or("?")
    )?;
    let database = &report["database"];
    if database.is_null() {
        writeln!(out, "Database: none yet")?;
    } else {
        let synced: u64 = database["synced_rows"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(_, rows)| rows.as_u64())
            .sum();
        writeln!(
            out,
            "Database: {} items, {synced} rows from a sync, {} db_info rows, Media anchor {}, {} playlists",
            database["items"],
            database["db_info_rows"],
            database["media_anchor"].as_str().unwrap_or("absent"),
            database["playlists"],
        )?;
        if let Some(songs) = database["leftover_songs"].as_u64().filter(|n| *n != 0) {
            writeln!(
                out,
                "{}",
                super::progress::leftover_rows_line(
                    songs,
                    database["songs_until_cleared"].as_u64()
                )
            )?;
        }
    }
    let readiness = &report["readiness"];
    let verdict = match readiness["state"].as_str() {
        Some("synced") if readiness["initialised_by_rustunes"] == true => {
            "initialised by rustunes; ready to sync".to_owned()
        }
        Some("synced") => "synced with a music library; ready to sync".to_owned(),
        Some("inconsistent") => format!(
            "not ready: the music library is inconsistent, and rustunes will not read it: {}",
            readiness["reason"].as_str().unwrap_or("?")
        ),
        Some("interrupted_sync") => "not ready: an interrupted sync left files on the iPhone that \
             stop rustunes from changing its music. If a rustunes command was interrupted, run it \
             again: it finishes the job and removes these files"
            .to_owned(),
        Some("never_synced") => {
            "never synced with a music library. Run rustunes init to prepare it".to_owned()
        }
        Some("init_unfinished") => {
            "rustunes init started but did not finish. Run rustunes init again".to_owned()
        }
        _ => format!(
            "unclear, rustunes will not initialise it:\n  - {}",
            readiness["reasons"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n  - ")
        ),
    };
    writeln!(out, "Verdict: {verdict}")?;
    Ok(())
}

#[cfg(test)]
mod tests;
