//! Terminal rendering of `rustunes pull`.
use anyhow::Result;

use crate::engine::{PullResult, PulledTrack};

/// Unbound tracks listed in full; the rest are counted.
const LISTED: usize = 25;

pub(super) fn render(report: &PullResult, library_set: bool, json: bool) -> Result<()> {
    if super::machine::enabled() {
        if !report.installed {
            super::machine::outcome("dry_run");
        }
        if !library_set {
            super::machine::notice(
                "no_library",
                super::machine::Level::Warning,
                "No library directory is set, so phone tracks were not matched to files; set one with `rustunes config library DIR` and pull again.",
                serde_json::json!({}),
            )?;
        }
        return super::machine::report("pull", report);
    }
    if json {
        serde_json::to_writer_pretty(std::io::stdout().lock(), report)?;
        println!();
        return Ok(());
    }
    println!(
        "The phone holds {} tracks. The journal now binds {} of them:",
        report.phone_tracks,
        report.carried + report.matched + report.copied.len()
    );
    if let Some(earlier) = &report.earlier {
        println!(
            "  {} carried from the earlier journal{} ({})",
            report.carried,
            if report.superseded {
                ", which another version of rustunes wrote"
            } else {
                ""
            },
            earlier.display()
        );
    }
    if report.matched > 0 || report.earlier.is_none() {
        println!("  {} matched to library files", report.matched);
    }
    if !report.copied.is_empty() {
        println!("  {} copied from the phone:", report.copied.len());
        for track in &report.copied {
            if let Some(path) = &track.copied_to {
                println!("    {} {}", track.track_id, path.display());
            }
        }
    }
    if !report.unbound.is_empty() {
        println!(
            "{} phone tracks are left as they are; sync neither changes nor removes them:",
            report.unbound.len()
        );
        for track in report.unbound.iter().take(LISTED) {
            println!("  {}", describe(track));
        }
        if report.unbound.len() > LISTED {
            println!(
                "  ... and {} more (see {})",
                report.unbound.len() - LISTED,
                report.run.join("pull.json").display()
            );
        }
        if report
            .unbound
            .iter()
            .any(|t| t.reason.as_deref() == Some(crate::engine::NOT_IN_LIBRARY))
        {
            println!(
                "`rustunes pull --tracks` copies those no library file holds into the library."
            );
        }
    }
    if report.gone > 0 {
        println!(
            "{} tracks the earlier journal bound are no longer on the phone.",
            report.gone
        );
    }
    if let Some(covers) = covers_text(report.covers_proven, report.covers_unproven) {
        println!("{covers}");
    }
    for item in &report.unfinished {
        println!("The earlier journal had {item}; the new journal holds what the phone shows now.");
    }
    for item in &report.unreadable {
        println!("Not carried: {item}");
    }
    if !library_set {
        println!(
            "No library directory is set, so phone tracks were not matched to files; set one with `rustunes config library DIR` and pull again."
        );
    }
    if report.installed {
        println!("Journal: {}", report.journal.display());
        if report.earlier.as_deref() == Some(report.journal.as_path()) {
            println!(
                "The journal it replaced is kept as {}.",
                report.journal.with_file_name("journal.previous").display()
            );
        }
    } else {
        println!(
            "Dry run: the journal in use is unchanged; the pulled journal is {}.",
            report.journal.display()
        );
    }
    Ok(())
}

fn describe(track: &PulledTrack) -> String {
    let name = match (track.artist.is_empty(), track.title.is_empty()) {
        (_, true) => "(untitled)".to_owned(),
        (true, false) => track.title.clone(),
        (false, false) => format!("{} - {}", track.artist, track.title),
    };
    match &track.reason {
        Some(reason) => format!("{} {name}: {reason}", track.track_id),
        None => format!("{} {name}", track.track_id),
    }
}

/// What a pull's cover readbacks found, when it read any.
pub(super) fn covers_text(proven: usize, unproven: usize) -> Option<String> {
    let mut lines = Vec::new();
    if proven > 0 {
        lines.push(format!(
            "{proven} phone covers read back as the expected image; the journal records them."
        ));
    }
    if unproven > 0 {
        lines.push(format!(
            "{unproven} phone covers could not be proven; the next sync sends them again, one phone session each."
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}
