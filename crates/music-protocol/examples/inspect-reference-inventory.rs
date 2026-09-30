//! Offline saved database inspection; full IDs require an explicit local output.
use music_protocol::catalogue::inventory;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use plist::{Dictionary, Value};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Read,
    path::PathBuf,
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    database: PathBuf,
    /// Write full inventory to a new local XML plist; refuses overwrite.
    #[arg(long)]
    output: Option<PathBuf>,
}

fn dictionary(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    Value::Dictionary(entries.into_iter().collect())
}

fn number(value: impl Into<plist::Integer>) -> Value {
    Value::Integer(value.into())
}

fn pid(value: u64) -> Value {
    Value::String(format!("{value:016X}"))
}

fn collection(row: &inventory::Collection) -> Value {
    dictionary([
        ("local_id".into(), number(row.local_id)),
        ("pid".into(), pid(row.pid)),
        ("kind".into(), number(u32::from(row.kind))),
        (
            "text".into(),
            dictionary(
                row.text
                    .iter()
                    .map(|(kind, value)| (kind.to_string(), Value::String(value.clone()))),
            ),
        ),
    ])
}

fn track(row: &inventory::Track) -> Value {
    let mut metadata = Dictionary::new();
    for (name, value) in [
        ("total_time_ms", row.metadata.total_time_ms),
        ("track_number", row.metadata.track_number),
        ("track_count", row.metadata.track_count),
        ("disc_number", row.metadata.disc_number),
        ("disc_count", row.metadata.disc_count),
        ("year", row.metadata.year),
        ("is_compilation", u32::from(row.metadata.is_compilation)),
    ] {
        metadata.insert(name.into(), number(value));
    }
    metadata.insert("file_size".into(), number(row.metadata.file_size));
    for (name, value) in [
        ("title", row.title()),
        ("artist", row.artist()),
        ("album", row.album()),
        ("album_artist", row.album_artist()),
    ] {
        if let Some(value) = value {
            metadata.insert(name.into(), Value::String(value.into()));
        }
    }
    let mut result = Dictionary::new();
    for (name, value) in [
        ("dataset", row.dataset),
        ("record_id", row.record_id),
        ("album_local_id", row.album_local_id),
        ("artist_local_id", row.artist_local_id),
    ] {
        result.insert(name.into(), number(value));
    }
    result.insert("pid".into(), pid(row.pid));
    result.insert("metadata".into(), Value::Dictionary(metadata));
    if let Some(value) = row.album_pid {
        result.insert("album_pid".into(), pid(value));
    }
    if let Some(value) = row.artist_pid {
        result.insert("artist_pid".into(), pid(value));
    }
    Value::Dictionary(result)
}

fn report(state: &inventory::Inventory, music_paths: BTreeSet<String>) -> Value {
    dictionary([
        ("music_paths".into(), Value::Array(music_paths.into_iter().map(Value::String).collect())),
        ("database_pid".into(), pid(state.database_pid())),
        ("primary_container_pid".into(), pid(state.primary_container_pid())),
        ("playlist_pids".into(), Value::Array(state.playlist_pids().iter().copied().map(pid).collect())),
        ("saved_media_anchor".into(), number(state.saved_media_anchor())),
        ("track_datasets".into(), dictionary(state.track_datasets().iter().map(|(kind, rows)|
            (kind.to_string(), Value::Array(rows.iter().map(track).collect()))))),
        ("album_by_local_id".into(), dictionary(state.albums().iter().map(|(id, row)|
            (id.to_string(), collection(row))))),
        ("artist_by_local_id".into(), dictionary(state.artists().iter().map(|(id, row)|
            (id.to_string(), collection(row))))),
        ("track_pids".into(), Value::Array(state.track_pids().iter().copied().map(pid).collect())),
        ("all_mapped_pids".into(), Value::Array(state.all_mapped_pids().iter().copied().map(pid).collect())),
        ("scope".into(), Value::String("Saved copy; freshness and deletion ownership are not established. Unknown payload semantics and collection grouping rules remain unparsed.".into())),
    ])
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input = File::open(&args.database).context("opening local database copy")?;
    let before = input.metadata().context("reading local file metadata")?;
    ensure!(before.is_file(), "database must be a local regular file");
    ensure!(
        before.len() <= inventory::MAX_INPUT_BYTES as u64,
        "database exceeds input limit"
    );
    let mut bytes = Vec::new();
    input
        .take(inventory::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("reading bounded local database")?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "database size changed during read"
    );
    let state = inventory::parse(&bytes)?;
    let music_paths = state.music_paths()?;
    let path_count = music_paths.len();
    if let Some(path) = args.output {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let output = options
            .open(path)
            .context("creating new private inventory output")?;
        report(&state, music_paths)
            .to_writer_xml(output)
            .context("writing private inventory plist")?;
    }
    println!(
        "Primary tracks: {}; track PID union: {}; albums: {}; artists: {}",
        state.primary_tracks().len(),
        state.track_pids().len(),
        state.albums().len(),
        state.artists().len()
    );
    println!("Saved Media host anchor: {}", state.saved_media_anchor());
    println!("Catalog Music paths: {path_count}");
    println!("This saved copy does not establish a live device anchor or deletion ownership.");
    Ok(())
}
