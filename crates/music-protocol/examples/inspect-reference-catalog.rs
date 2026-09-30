//! Offline-only inspection of an explicitly supplied Music SQLite capture.
use music_protocol::catalogue::sqlite;

use anyhow::{Context, Result};
use clap::Parser;
use plist::{Dictionary, Value};
use std::{fs::OpenOptions, path::PathBuf};

#[derive(Parser)]
struct Args {
    database: PathBuf,
    #[arg(
        long,
        required_unless_present = "without_wal",
        conflicts_with = "without_wal"
    )]
    wal: Option<PathBuf>,
    /// Explicitly attest that this capture has no WAL.
    #[arg(long)]
    without_wal: bool,
    #[arg(long, default_value_os_t = std::env::temp_dir())]
    scratch_parent: PathBuf,
    /// Full private catalog is written only when this new output path is supplied.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn id(value: u64) -> Value {
    Value::String(format!("{value:016X}"))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let catalog = sqlite::read_snapshot(&args.database, args.wal.as_deref(), &args.scratch_parent)?;
    if let Some(output) = args.output {
        let mut root = Dictionary::new();
        root.insert("database_pid".into(), id(catalog.database_pid()));
        root.insert(
            "primary_container_pid".into(),
            id(catalog.primary_container_pid()),
        );
        root.insert(
            "persisted_media_anchor".into(),
            Value::Integer(catalog.persisted_media_anchor().into()),
        );
        root.insert(
            "database_sha256".into(),
            Value::String(hex(&catalog.source_hashes().database)),
        );
        if let Some(hash) = catalog.source_hashes().wal {
            root.insert("wal_sha256".into(), Value::String(hex(&hash)));
        }
        let tracks = catalog
            .tracks()
            .iter()
            .map(|t| {
                let mut d = Dictionary::new();
                d.insert("sync_id".into(), id(t.sync_id));
                d.insert("internal_id".into(), Value::Integer(t.internal_id.into()));
                d.insert("album_sync_id".into(), id(t.album_sync_id));
                d.insert("artist_sync_id".into(), id(t.artist_sync_id));
                let m = &t.metadata;
                for (key, value) in [
                    ("title", &m.title),
                    ("artist", &m.artist),
                    ("album", &m.album),
                    ("album_artist", &m.album_artist),
                ] {
                    d.insert(key.into(), Value::String(value.clone()));
                }
                for (key, value) in [
                    ("genre", &m.genre),
                    ("composer", &m.composer),
                    ("copyright", &m.copyright),
                    ("afc_path", &t.afc_path),
                ] {
                    if let Some(value) = value {
                        d.insert(key.into(), Value::String(value.clone()));
                    }
                }
                for (key, value) in [
                    ("track_number", m.track_number),
                    ("track_count", m.track_count),
                    ("disc_number", m.disc_number),
                    ("disc_count", m.disc_count),
                    ("year", m.year),
                    ("total_time_ms", m.total_time_ms),
                ] {
                    d.insert(key.into(), Value::Integer(value.into()));
                }
                d.insert("file_size".into(), Value::Integer(m.file_size.into()));
                for (key, value) in [
                    ("is_placeholder", t.availability.is_placeholder),
                    ("is_library_added", t.availability.is_library_added),
                    (
                        "has_non_purgeable_asset",
                        t.availability.has_non_purgeable_asset,
                    ),
                    ("is_supported_type", t.availability.is_supported_type),
                ] {
                    d.insert(key.into(), Value::Boolean(value));
                }
                Value::Dictionary(d)
            })
            .collect();
        root.insert("tracks".into(), Value::Array(tracks));
        root.insert(
            "represented_sync_ids".into(),
            Value::Array(
                catalog
                    .represented_sync_ids()
                    .iter()
                    .copied()
                    .map(id)
                    .collect(),
            ),
        );
        root.insert(
            "albums".into(),
            Value::Array(
                catalog
                    .albums()
                    .values()
                    .map(|a| {
                        let mut d = Dictionary::new();
                        d.insert("internal_id".into(), Value::Integer(a.internal_id.into()));
                        d.insert("sync_id".into(), id(a.sync_id));
                        d.insert("name".into(), Value::String(a.name.clone()));
                        d.insert(
                            "artist_internal_id".into(),
                            Value::Integer(a.artist_internal_id.into()),
                        );
                        d.insert("artist_sync_id".into(), id(a.artist_sync_id));
                        d.insert(
                            "live_track_count".into(),
                            Value::Integer((a.live_track_count as u64).into()),
                        );
                        Value::Dictionary(d)
                    })
                    .collect(),
            ),
        );
        root.insert(
            "artists".into(),
            Value::Array(
                catalog
                    .artists()
                    .values()
                    .map(|a| {
                        let mut d = Dictionary::new();
                        d.insert("internal_id".into(), Value::Integer(a.internal_id.into()));
                        d.insert("sync_id".into(), id(a.sync_id));
                        d.insert("name".into(), Value::String(a.name.clone()));
                        d.insert(
                            "live_track_count".into(),
                            Value::Integer((a.live_track_count as u64).into()),
                        );
                        Value::Dictionary(d)
                    })
                    .collect(),
            ),
        );
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(output)
            .context("create new private catalog output")?;
        Value::Dictionary(root).to_writer_binary(file)?;
    }
    println!(
        "tracks={} albums={} artists={} persisted_media_anchor={}",
        catalog.tracks().len(),
        catalog.albums().len(),
        catalog.artists().len(),
        catalog.persisted_media_anchor()
    );
    Ok(())
}
