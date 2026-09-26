//! Offline-only fingerprinting of explicitly supplied local files.
use music_protocol::media::fingerprint;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use plist::{Dictionary, Value};
use std::{fs::OpenOptions, path::PathBuf};

#[derive(Parser)]
struct Args {
    #[arg(required = true)]
    sources: Vec<PathBuf>,
    /// Write private fingerprints to this new file; never replace an existing file.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn hex(bytes: &[u8]) -> Value {
    Value::String(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.sources.len() <= 1024, "too many explicit source paths");
    let mut records = Vec::new();
    for path in &args.sources {
        let result = fingerprint::read(path)?;
        let mut record = Dictionary::new();
        record.insert(
            "path".into(),
            path.to_str()
                .context("inspection output requires UTF-8 source paths")?
                .into(),
        );
        record.insert("file_bytes".into(), result.file_bytes.into());
        record.insert("full_file_sha256".into(), hex(&result.full_file_sha256));
        for (name, digest) in [
            ("encoded_alac_sha256", result.audio.sha256),
            ("alac_config_sha256", result.audio.alac_config_sha256),
            ("sample_sizes_sha256", result.audio.sample_sizes_sha256),
            ("media_sha256", result.audio.media_sha256),
        ] {
            record.insert(name.into(), hex(&digest));
        }
        record.insert("timescale".into(), result.audio.timescale.into());
        record.insert("duration_ticks".into(), result.audio.duration_ticks.into());
        record.insert("media_bytes".into(), result.audio.media_bytes.into());
        records.push(Value::Dictionary(record));
    }
    if let Some(path) = args.output {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        Value::Array(records).to_writer_binary(&mut file)?;
        file.sync_all()?;
    }
    println!("sources={}", args.sources.len());
    Ok(())
}
