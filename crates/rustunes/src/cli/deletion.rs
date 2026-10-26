//! User-facing removal arguments, presentation and explicit confirmation.
use crate::engine::execution::delete::{self, Confirmation, Event};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
pub(super) struct Options {
    /// Save an exact removal proposal without changing the phone or journal.
    #[arg(long)]
    dry_run: bool,
    /// Reuse a complete snapshot; execution rechecks current device state.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Existing per-device journal. Defaults to this tool's private state.
    #[arg(long)]
    journal: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Also save the full selection and disclosed side effects to this new file.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Confirm the exact printed selection without an interactive prompt.
    #[arg(long)]
    yes: bool,
    /// Emit one JSON event per line; progress remains on stderr.
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "rustunes")]
    host_name: String,
    /// Maximum seconds for each complete audio verification pass.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
    seconds: u64,
    /// Operations per native session besides its update_db_info, at most and by default 499 (one operation file): each removed track is one; each batch is verified before the next.
    #[arg(long, default_value_t = super::DEFAULT_BATCH, value_parser = clap::value_parser!(u64).range(1..=super::MAX_BATCH))]
    batch_size: u64,
}

impl Options {
    pub(super) fn into_request(self, state_dir: PathBuf) -> delete::Options {
        delete::Options {
            dry_run: self.dry_run,
            snapshot: self.snapshot,
            journal: self.journal,
            state_dir,
            output: self.output,
            host_name: self.host_name,
            seconds: self.seconds,
            batch_size: self.batch_size as usize,
            leave_for_sync: false,
            pinned: None,
        }
    }
    pub(super) fn state_dir(&self) -> Option<&Path> {
        self.state_dir.as_deref()
    }
    pub(super) fn json(&self) -> bool {
        self.json
    }
    pub(super) fn yes(&self) -> bool {
        self.yes
    }
}
pub(super) fn confirm_selection(selection: Confirmation<'_>, yes: bool) -> Result<bool> {
    if yes { Ok(true) } else { confirm(selection) }
}
/// `sync`'s confirmation of the tracks whose source file left the library.
pub(super) fn confirm_mirror(selection: Confirmation<'_>, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if super::machine::enabled() {
        return confirm(selection);
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "sync would remove {} whose source files are gone from the library; confirm with --yes, inspect with --dry-run, or run `rustunes push` to send new tracks without removing any",
        tracks(selection.count)
    );
    eprintln!(
        "Their source files are gone from the library, so sync removes them from the phone. To send new tracks without removing any, run `rustunes push` instead."
    );
    confirm(selection)
}
/// "this track" or "these N tracks".
fn tracks(count: usize) -> String {
    match count {
        1 => "this track".into(),
        n => format!("these {n} tracks"),
    }
}
fn confirm(selection: Confirmation<'_>) -> Result<bool> {
    let playlists = selection.playlists;
    let count = tracks(selection.count);
    let prompt = match playlists {
        0 => format!("Remove {count} from this iPhone?"),
        _ => {
            format!("Remove {count} and then delete these {playlists} playlists from this iPhone?")
        }
    };
    if super::machine::enabled() {
        return super::machine::confirm(
            "remove_tracks",
            prompt,
            json!({
                "tracks": selection.count,
                "playlists": playlists,
                "selection": selection.selection,
            }),
        );
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "noninteractive deletion requires --yes; use --dry-run to inspect the selection"
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

fn strings(value: &Value) -> Result<Vec<&str>> {
    value
        .as_array()
        .context("selection list absent")?
        .iter()
        .map(|v| v.as_str().context("selection string absent"))
        .collect()
}
fn render_selection(descriptor: &Value, path: &Path, json: bool) -> Result<()> {
    let plan = &descriptor["proposal"];
    let selected_count = plan["proposed_removal_ids"]
        .as_array()
        .context("selected IDs absent")?
        .len();
    let reset_count = descriptor["recent_counter_resets_on_survivors"]
        .as_array()
        .context("reset facts absent")?
        .len();
    if json {
        serde_json::to_writer(std::io::stdout().lock(), descriptor)?;
        println!();
    } else {
        println!(
            "{}: {} selected removals, {} preserved; {} bytes of catalogue audio selected.",
            plan["mode"].as_str().context("selection mode absent")?,
            selected_count,
            plan["preserved_ids"]
                .as_array()
                .context("preserved IDs absent")?
                .len(),
            plan["affected_audio_bytes"]
                .as_u64()
                .context("selection bytes absent")?
        );
        for row in plan["tracks"].as_array().context("selection rows absent")? {
            match row["decision"]
                .as_str()
                .context("selection decision absent")?
            {
                "proposed_removal" => println!(
                    "SELECTED {} {:?} — {:?} [{}]{}",
                    row["track_id"].as_str().context("track ID absent")?,
                    row["title"].as_str().context("title absent")?,
                    row["artist"].as_str().context("artist absent")?,
                    row["journal_origin"].as_str().context("origin absent")?,
                    row["missing_source_path"]
                        .as_str()
                        .map(|path| format!(" source gone: {path}"))
                        .unwrap_or_default()
                ),
                "blocked" => {
                    println!(
                        "BLOCKED {}: {}",
                        row["track_id"].as_str().context("track ID absent")?,
                        strings(&row["reasons"])?.join(", ")
                    )
                }
                _ => {}
            }
        }
        for reason in strings(&plan["blockers"])? {
            println!("BLOCKED: {reason}");
        }
        if let Some(sequence) = descriptor
            .get("sql_sequence_preview")
            .filter(|v| !v.is_null())
        {
            for reason in strings(&sequence["blockers"])? {
                println!("BLOCKED: {reason}");
            }
            for step in sequence["steps"]
                .as_array()
                .context("sequence steps absent")?
            {
                for reason in strings(&step["blockers"])? {
                    println!(
                        "BLOCKED {:016x}: {reason}",
                        step["track_pid"].as_u64().context("step track absent")?
                    );
                }
            }
        }
        if let Some(playlists) = descriptor.get("playlists") {
            for (key, label) in [
                ("deleted_after_tracks", "PLAYLIST DELETED"),
                ("kept", "PLAYLIST KEPT"),
            ] {
                for row in playlists[key].as_array().context("playlist list absent")? {
                    println!(
                        "{label} {} {:?}",
                        row["container"].as_str().context("playlist ID absent")?,
                        row["name"].as_str().context("playlist name absent")?
                    );
                }
            }
            for reason in strings(&playlists["blockers"])? {
                println!("BLOCKED: {reason}");
            }
        }
        if let Some(missing) = descriptor.get("missing_sources") {
            let unknown = missing["source_path_unknown"]
                .as_array()
                .context("unattributed tracks absent")?;
            if let Some(first) = unknown.first() {
                println!(
                    "Kept {} tracks this tool inserted whose source path the journal does not name (first: {}: {}).",
                    unknown.len(),
                    first["track_id"].as_str().context("track ID absent")?,
                    first["reason"].as_str().context("reason absent")?
                );
            }
            for row in plan["tracks"].as_array().context("selection rows absent")? {
                if row["decision"] == "preserved"
                    && let Some(path) = row["missing_source_path"].as_str()
                {
                    println!(
                        "KEPT {} {:?}: source gone from {path}, but its audio is in the selection at {}",
                        row["track_id"].as_str().context("track ID absent")?,
                        row["title"].as_str().context("title absent")?,
                        strings(&row["matching_source_paths"])?.join(", ")
                    );
                }
            }
        }
        if reset_count != 0 {
            println!(
                "Import also resets recent counters on {} surviving tracks; lifetime counts are preserved. Exact values are in the saved plan.",
                reset_count
            );
        }
        println!("Selection: {}", path.display());
    }
    Ok(())
}
/// What a finished removal prints on stdout.
fn removals(n: usize) -> String {
    match n {
        1 => "1 removal".into(),
        n => format!("{n} removals"),
    }
}

pub(super) fn completed_line(
    removed: usize,
    deleted_playlists: usize,
    left: usize,
    evidence: Option<&Path>,
) -> String {
    match (evidence, deleted_playlists) {
        (None, _) => "Nothing to remove.".into(),
        (Some(evidence), _) if removed == 0 && left > 0 => format!(
            "{} left to the sync, with its first new tracks or alone. Evidence: {}",
            removals(left),
            evidence.display()
        ),
        (Some(evidence), _) if left > 0 => format!(
            "Removed and verified {removed} tracks, leaving {} to the sync, with its first new tracks or alone. Evidence: {}",
            removals(left),
            evidence.display()
        ),
        (Some(evidence), 0) => format!(
            "Removed and verified {removed} tracks. Evidence: {}",
            evidence.display()
        ),
        (Some(evidence), _) => format!(
            "Removed and verified {removed} tracks and deleted {deleted_playlists} playlists. Evidence: {}",
            evidence.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removals_left_to_the_sync_are_counted_in_words() {
        let evidence = Path::new("/run");
        assert_eq!(
            completed_line(0, 0, 1, Some(evidence)),
            "1 removal left to the sync, with its first new tracks or alone. Evidence: /run"
        );
        assert_eq!(
            completed_line(499, 0, 101, Some(evidence)),
            "Removed and verified 499 tracks, leaving 101 removals to the sync, with its first new tracks or alone. Evidence: /run"
        );
    }
}
