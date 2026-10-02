//! Offline builder only. No device I/O or authorization follows from a plist.

use music_protocol::catalogue::sqlite;

use anyhow::{Result, ensure};
use clap::Parser;
use music_protocol::catalogue::metadata_update;
use std::{
    fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::SystemTime,
};
#[derive(Parser)]
struct Args {
    database: PathBuf,
    #[arg(
        long,
        required_unless_present = "without_wal",
        conflicts_with = "without_wal"
    )]
    wal: Option<PathBuf>,
    #[arg(long)]
    without_wal: bool,
    #[arg(long)]
    target: String,
    #[arg(long)]
    composer: Option<String>,
    #[arg(long)]
    genre: Option<String>,
    #[arg(long)]
    disc_number: Option<u16>,
    #[arg(long)]
    disc_count: Option<u16>,
    #[arg(long)]
    track_number: Option<u16>,
    #[arg(long)]
    track_count: Option<u16>,
    #[arg(long)]
    year: Option<u16>,
    #[arg(long)]
    output: PathBuf,
}
fn main() -> Result<()> {
    let a = Args::parse();
    ensure!(
        a.target.len() == 16 && a.target.bytes().all(|b| b.is_ascii_hexdigit()),
        "target must be exactly 16 hexadecimal digits"
    );
    let c = sqlite::read_snapshot(&a.database, a.wal.as_deref(), &std::env::temp_dir())?;
    let p = metadata_update::ScalarPatch {
        composer: a.composer,
        genre: a.genre,
        disc_number: a.disc_number,
        disc_count: a.disc_count,
        track_number: a.track_number,
        track_count: a.track_count,
        year: a.year,
    };
    let bytes = metadata_update::prepare(
        &c,
        u64::from_str_radix(&a.target, 16)?,
        &p,
        SystemTime::now(),
    )?;
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(a.output)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    println!(
        "Prepared explicit scalar patch offline; receiver qualification and full preservation evidence remain required."
    );
    Ok(())
}
