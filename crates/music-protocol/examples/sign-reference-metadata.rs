//! Local-only candidate signer. This example has no device transport.

use music_protocol::protocol::signing as grappa;

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;

#[derive(Parser)]
#[command(
    about = "Create a local candidate Grappa sidecar for the fixed public host request; device acceptance is unproven"
)]
struct Args {
    /// Exact 84-byte Grappa request used for this session.
    #[arg(long)]
    request: PathBuf,
    /// Exact 83-byte ReadyToSync Grappa reply from the same session.
    #[arg(long)]
    reply: PathBuf,
    /// Exact serialized operation bytes, between 1 byte and 4 MiB.
    #[arg(long)]
    operation: PathBuf,
    /// New local output file. Existing files are never overwritten.
    #[arg(long)]
    output: PathBuf,
}

fn bounded_read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let file = File::open(path).context("open local signer input")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "signer input must be a regular file");
    ensure!(
        metadata.len() <= cap as u64,
        "signer input exceeds its size limit"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= cap,
        "signer input grew beyond its size limit"
    );
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "signer input size changed while reading"
    );
    Ok(bytes)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let request = bounded_read(&args.request, 84)?;
    let reply = bounded_read(&args.reply, 83)?;
    let operation = bounded_read(&args.operation, grappa::MAX_OPERATION_BYTES)?;
    let sidecar = grappa::FixedHostSession::from_reply(&request, &reply)?.sign(&operation)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.output)
        .context("create new local sidecar")?;
    output.write_all(&sidecar)?;
    output.sync_all()?;
    println!("Created 21-byte local candidate sidecar. No device operation occurred.");
    Ok(())
}
