//! Offline only: validated single-track ALAC PCM comparison evidence.

use music_protocol::media::fingerprint;
use music_protocol::media::pcm as pcm_fingerprint;
use music_protocol::media::timing;

use anyhow::Result;
use clap::Parser;
use std::{fs::File, path::PathBuf};

#[derive(Parser)]
#[command(about = "Read-only PCM identity for validated 44100 Hz 16-bit stereo ALAC")]
struct Args {
    source: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Prove self-contained encoded input before this optional decoding fallback.
    let source = fingerprint::read(&args.source)?;
    let timing = timing::read(&mut File::open(&args.source)?)?;
    let decoded = pcm_fingerprint::read(&args.source, &timing)?;
    anyhow::ensure!(
        source.file_bytes == decoded.file_bytes
            && source.full_file_sha256 == decoded.full_file_sha256,
        "source changed between encoded and decoded inspection"
    );
    println!("Profile: decoded-pcm / 44100 Hz / 16 bit / stereo / interleaved s16le");
    println!("File bytes: {}", decoded.file_bytes);
    println!("File SHA256: {}", hex(&decoded.full_file_sha256));
    println!("PCM bytes: {}", decoded.pcm.pcm_bytes);
    println!("Samples per channel: {}", decoded.pcm.samples_per_channel);
    println!("PCM SHA256: {}", hex(&decoded.pcm.pcm_sha256));
    println!(
        "Profile-bound identity SHA256: {}",
        hex(&decoded.pcm.sha256)
    );
    Ok(())
}

fn hex(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
