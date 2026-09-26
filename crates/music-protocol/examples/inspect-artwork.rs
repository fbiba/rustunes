//! Offline source-cover inspection. Does not connect to the phone or write images.
use music_protocol::media::artwork;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    source: PathBuf,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn main() -> Result<()> {
    let source = artwork::read(&Args::parse().source)?;
    println!("Source bytes: {}", source.source_size);
    println!("Source SHA-256: {}", hex(&source.source_sha256));
    match source.cover {
        Some(cover) => {
            println!("Embedded JPEG bytes: {}", cover.bytes.len());
            println!("Embedded JPEG SHA-256: {}", hex(&cover.sha256));
            println!("Native artwork checksum: {}", cover.checksum);
        }
        None => println!("Embedded artwork: absent"),
    }
    Ok(())
}
