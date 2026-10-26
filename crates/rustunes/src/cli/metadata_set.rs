//! `metadata set`: explicit field edits sent as one verified metadata batch.
use crate::engine::{execution::metadata_batch, planning::metadata_edit::EditRequest};
use anyhow::{Result, ensure};
use clap::Args;
use music_protocol::catalogue::metadata_edit::TrackUpdate;
use plist::Value;
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

/// The fields to change; at least one is required. A changed name without its
/// sort name also sets the sort name to the new name.
#[derive(Args)]
#[group(required = true, multiple = true)]
pub(super) struct Fields {
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    sort_title: Option<String>,
    #[arg(long)]
    artist: Option<String>,
    #[arg(long)]
    sort_artist: Option<String>,
    #[arg(long)]
    album: Option<String>,
    #[arg(long)]
    sort_album: Option<String>,
    #[arg(long)]
    album_artist: Option<String>,
    #[arg(long)]
    sort_album_artist: Option<String>,
    #[arg(long)]
    genre: Option<String>,
    #[arg(long)]
    composer: Option<String>,
    #[arg(long)]
    sort_composer: Option<String>,
    /// 0 clears it.
    #[arg(long, value_parser = clap::value_parser!(u16).range(0..=9999))]
    year: Option<u16>,
    /// 0 clears it.
    #[arg(long)]
    pub(super) track_number: Option<u16>,
    /// 0 clears it.
    #[arg(long)]
    track_count: Option<u16>,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    disc_number: Option<u16>,
    /// 0 clears it.
    #[arg(long)]
    disc_count: Option<u16>,
    #[arg(long)]
    compilation: Option<bool>,
    /// 0 none, 1 explicit, 2 clean.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=2))]
    content_rating: Option<u8>,
    /// An empty copyright clears it.
    #[arg(long)]
    copyright: Option<String>,
    /// An empty comment clears it.
    #[arg(long)]
    comment: Option<String>,
}

impl Fields {
    pub(super) fn request(self, track: u64) -> EditRequest {
        EditRequest {
            track,
            title: self.title,
            sort_title: self.sort_title,
            artist: self.artist,
            sort_artist: self.sort_artist,
            album: self.album,
            sort_album: self.sort_album,
            album_artist: self.album_artist,
            sort_album_artist: self.sort_album_artist,
            genre: self.genre,
            composer: self.composer,
            sort_composer: self.sort_composer,
            year: self.year,
            track_number: self.track_number,
            track_count: self.track_count,
            disc_number: self.disc_number,
            disc_count: self.disc_count,
            is_compilation: self.compilation,
            content_rating: self.content_rating,
            copyright: self.copyright,
            comment: self.comment,
        }
    }
}

pub(super) struct Request {
    pub(super) request: EditRequest,
    pub(super) snapshot: Option<PathBuf>,
    pub(super) journal: Option<PathBuf>,
    pub(super) state: PathBuf,
    pub(super) host_name: String,
    pub(super) seconds: u64,
    pub(super) dry_run: bool,
    pub(super) yes: bool,
}

pub(super) async fn run(r: Request, engine: &crate::engine::Engine) -> Result<bool> {
    let yes = r.yes;
    let connection = engine.command_options();
    let outcome = metadata_batch::set(
        metadata_batch::SetOptions {
            requests: vec![r.request],
            snapshot: r.snapshot.as_deref(),
            journal: r.journal.as_deref(),
            state: &r.state,
            host_name: &r.host_name,
            dry_run: r.dry_run,
            connection: &connection,
        },
        || engine.control(r.seconds),
        show,
        |count| confirm(count, yes),
    )
    .await?;
    let (name, line) = match outcome {
        metadata_batch::SetOutcome::Unchanged => ("nothing_to_do", "Nothing to change.".into()),
        metadata_batch::SetOutcome::Preview => ("dry_run", "Dry run; nothing was sent.".into()),
        metadata_batch::SetOutcome::Cancelled => {
            ("cancelled", "Cancelled; nothing was sent.".into())
        }
        metadata_batch::SetOutcome::Applied { updated } => (
            "completed",
            format!("Verified the edit of {updated} track(s) on the phone."),
        ),
    };
    if super::machine::enabled() {
        let updated = match outcome {
            metadata_batch::SetOutcome::Applied { updated } => updated,
            _ => 0,
        };
        super::machine::outcome(name);
        super::machine::report(
            "metadata_set",
            &serde_json::json!({"outcome": name, "updated": updated, "message": line}),
        )?;
    } else {
        println!("{line}");
    }
    Ok(!matches!(outcome, metadata_batch::SetOutcome::Cancelled))
}

fn show(updates: &[TrackUpdate], operation: &Path) -> Result<()> {
    if super::machine::enabled() {
        return super::machine::report(
            "metadata_edit",
            &serde_json::json!({
                "tracks": updates.iter().map(|update| serde_json::json!({
                    "track_id": format!("{:016x}", update.sync_id),
                    "fields": update
                        .item
                        .iter()
                        .map(|(key, value)| (key.clone(), super::plist_json(value)))
                        .collect::<serde_json::Map<_, _>>(),
                })).collect::<Vec<_>>(),
                "operation": (!updates.is_empty()).then_some(operation),
            }),
        );
    }
    for update in updates {
        println!("{:016x}", update.sync_id);
        for (key, value) in &update.item {
            println!("  {key} = {}", text(value));
        }
    }
    if !updates.is_empty() {
        println!("Operation: {}", operation.display());
    }
    Ok(())
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        Value::Integer(n) => n
            .as_signed()
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("{n:?}")),
        Value::Boolean(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn confirm(count: usize, yes: bool) -> Result<bool> {
    if yes {
        if super::machine::enabled() {
            super::machine::phase(
                "metadata",
                Some("Sending the edit in one native session".into()),
            )?;
        }
        return Ok(true);
    }
    let prompt = format!("Send this edit of {count} track(s) to the iPhone?");
    if super::machine::enabled() {
        let yes =
            super::machine::confirm("send_edit", prompt, serde_json::json!({"tracks": count}))?;
        if yes {
            super::machine::phase(
                "metadata",
                Some("Sending the edit in one native session".into()),
            )?;
        }
        return Ok(yes);
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "a noninteractive edit requires --yes; use --dry-run to see it"
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
