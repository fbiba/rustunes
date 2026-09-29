//! Offline saved Media host-anchor inspection. This example has no device code.

use music_protocol::catalogue::anchor;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use std::{fs::File, io::Read, path::PathBuf};

#[derive(Parser)]
#[command(
    about = "Inspect saved Media host state in a local iTunesDB/CDB copy; not a current device anchor",
    arg_required_else_help = true
)]
struct Args {
    /// Local regular database file, at most 64 MiB.
    #[arg(long)]
    database: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let file = File::open(&args.database).context("opening copied reference database")?;
    let metadata = file
        .metadata()
        .context("reading reference database metadata")?;
    ensure!(
        metadata.is_file() && metadata.len() <= anchor::MAX_INPUT_BYTES as u64,
        "reference database must be a regular file at most 64 MiB"
    );
    let mut bytes = Vec::new();
    file.take(anchor::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("reading bounded reference database")?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "reference database size changed during read"
    );
    let state = anchor::parse(&bytes)?;
    println!("Saved Media host anchor: {}", state.saved_media_anchor());
    println!(
        "Saved VoiceMemo host anchor: {}",
        state.saved_voice_memo_anchor()
    );
    println!("This copied value does not establish the current device anchor or a completed sync.");
    Ok(())
}
