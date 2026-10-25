pub(crate) mod completions;
pub(crate) mod config;
use crate::engine;
use crate::engine::execution::device;
use crate::engine::planning::additive;
use crate::engine::planning::source_collection;
use crate::engine::planning::source_reads;
use crate::engine::state::layout;
use crate::engine::state::snapshot;
use crate::engine::verification::audio_capture;
use crate::engine::verification::catalogue_capture;
use music_protocol::catalogue::identity;
use music_protocol::media::metadata;
use music_protocol::protocol::framing;
use music_protocol::protocol::trace::{private_file, private_write};

use std::{fs, io::Write, path::PathBuf, process::ExitCode, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use idevice::{
    IdeviceService,
    provider::IdeviceProvider,
    services::{lockdown::LockdownClient, syslog_relay::SyslogRelayClient},
};
use tokio::time::{Instant, timeout};

#[derive(Parser)]
#[command(
    version,
    about = "Inspect audio sources (ALAC, AAC, MP3, FLAC, WAV, AIFF) and plan qualified iPhone Music transfers over USB or Wi-Fi."
)]
struct Cli {
    /// Select an exact device identifier from device list.
    #[arg(long, global = true, value_name = "UDID")]
    device: Option<String>,
    /// Reach the phone over Wi-Fi instead of USB. It must have been paired and
    /// trusted over USB once, be on this network and be awake; it is found by
    /// Bonjour and secured with the pairing record usbmuxd saved.
    #[arg(long, global = true)]
    wifi: bool,
    /// Reach the phone over Wi-Fi at this address instead of finding it by
    /// Bonjour; implies --wifi.
    #[arg(long, global = true, value_name = "IP")]
    wifi_address: Option<std::net::IpAddr>,
    /// Copy every track's audio from the phone when indexing it, instead of
    /// reusing cached fingerprints of files whose size and times are unchanged,
    /// and hash every local source again. Slow (minutes per pass); refreshes
    /// both caches. Use for audits.
    #[arg(long, global = true)]
    full_readback: bool,
    /// How to print the command's output. `json` prints JSON records, one per
    /// line on stdout, for programs that drive rustunes: progress with
    /// estimates of the time left, the file or track in hand, notices,
    /// prompts answered on stdin, reports and the result. See
    /// docs/07-sync-engine-design.md, section 10.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "human",
        value_name = "FORMAT"
    )]
    message_format: MessageFormat,
    /// Print the run's log on stderr as well. Every run keeps its log in the
    /// state directory under logs; a failure keeps it under logs/failed and
    /// prints what the terminal did not show of it, and its path. -vv adds the
    /// debug output of the library that reaches the phone, which names the
    /// phone's identifiers, and -vvv everything. RUSTUNES_LOG (a filter such
    /// as `info,idevice=debug`) sets what is logged instead.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum MessageFormat {
    /// Sentences for a person, on stderr and stdout.
    Human,
    /// One JSON record per line on stdout.
    Json,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Set the library directory `sync` mirrors and `push` and `pull` default to; without DIR, print it.
    Library { directory: Option<PathBuf> },
}

#[derive(Subcommand)]
enum Command {
    /// Discover USB devices and inspect the selected paired phone.
    Device {
        #[command(subcommand)]
        view: DeviceCommand,
    },
    /// Inspect or prune the local fingerprint cache, without accessing a device.
    Cache {
        #[command(subcommand)]
        action: CacheCommand,
    },
    /// Reconcile an unfinished operation from retained evidence; never blindly replay a transfer.
    Resume {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        journal: PathBuf,
        /// Retained sync/apply/artwork run directory.
        #[arg(long, conflicts_with = "cancel_prepared")]
        run_dir: Option<PathBuf>,
        #[arg(long)]
        trace_dir: PathBuf,
        /// Cancel this exact local never-started attempt, retaining historical identity reservations.
        #[arg(long, value_parser=clap::value_parser!(i64).range(1..))]
        cancel_prepared: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Print shell completion to stdout (source the output in Bash).
    Completions {
        #[arg(value_parser = ["bash"])]
        shell: String,
    },
    /// Mirror the library directory onto the phone: send its new tracks, update changed tags and covers, and remove the tracks this tool sent whose source file has left the library (listed and confirmed first). Set the directory once with `rustunes config library DIR`; `rustunes push` only sends.
    Sync {
        /// Remove every track from the phone, then add the library again, as one resumable replacement.
        #[arg(long)]
        replace_library: bool,
        /// Remove the tracks this tool inserted whose source audio changed at the same path, or whose source cleared a genre or composer, then add them again.
        #[arg(long, conflicts_with = "replace_library")]
        replace_changed: bool,
        /// Confirm a replacement or removal without an interactive prompt.
        #[arg(long)]
        yes: bool,
        /// Private run history and per-device journals (default: XDG_STATE_HOME/rustunes).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Reuse a journal created by apply instead of the automatic per-device journal.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Save the plan and perform live read-only preflight; nothing is sent or removed.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "rustunes")]
        host_name: String,
        /// Maximum time for a new complete device snapshot.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
        seconds: u64,
        /// Operations per native session besides its update_db_info, at most and by default 499 (one operation file): each removed track is one, and each new track with its cover (at 1, each MP3 or other non-ALAC track still goes in an insert batch of its own); each batch is verified before the next, and any other change runs one per session afterwards.
        #[arg(long, default_value_t = DEFAULT_BATCH, value_parser = clap::value_parser!(u64).range(1..=MAX_BATCH))]
        batch_size: u64,
        /// Send each new FLAC as an ALAC converted from it with FFmpeg (checked first, before anything else); the conversion must decode to the FLAC's exact audio.
        #[arg(long)]
        flac_to_alac: bool,
        /// Mirror the files in the library's subfolders too; without it only the files directly in the library directory are mirrored.
        #[arg(long)]
        recursive: bool,
    },
    /// Send the new tracks of a directory or a single music file (default: the library directory), with verified audio and covers; tracks already on the phone keep their audio and metadata, and nothing is removed.
    Push {
        path: Option<PathBuf>,
        /// Private run history and per-device journals (default: XDG_STATE_HOME/rustunes).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Reuse a complete snapshot; fresh device state is checked before execution.
        #[arg(long)]
        snapshot: Option<PathBuf>,
        /// Reuse a journal created by apply instead of the automatic per-device journal.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Save the plan and perform live read-only preflight; nothing is sent.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "rustunes")]
        host_name: String,
        /// Maximum time for a new complete device snapshot.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
        seconds: u64,
        /// New tracks, with their covers, per native session, as for sync.
        #[arg(long, default_value_t = DEFAULT_BATCH, value_parser = clap::value_parser!(u64).range(1..=MAX_BATCH))]
        batch_size: u64,
        /// Send each new FLAC as an ALAC converted from it with FFmpeg, as for sync.
        #[arg(long)]
        flac_to_alac: bool,
        /// Send the files in subfolders too; without it only the files directly in the directory are sent.
        #[arg(long)]
        recursive: bool,
        /// Include an exact root-relative file or subtree; repeat to select more.
        #[arg(long, value_name = "PATH")]
        include: Vec<String>,
        /// Exclude an exact root-relative file or subtree; exclusions win.
        #[arg(long, value_name = "PATH")]
        exclude: Vec<String>,
    },
    /// Rebuild this host's journal from the phone's current library: carry forward what an earlier journal (even one that another version of rustunes wrote) knows, and bind each phone track a library file holds. Nothing is sent to the phone.
    Pull {
        /// Also copy each phone track no library file holds into the library directory as "Artist - Title", checked byte for byte, and bind it there.
        #[arg(long)]
        tracks: bool,
        /// Copy --tracks into this directory instead of the library directory.
        #[arg(long, value_name = "DIR", requires = "tracks")]
        into: Option<PathBuf>,
        /// Match phone tracks to the files in the library's subfolders too, as `sync --recursive` mirrors them.
        #[arg(long)]
        recursive: bool,
        /// Report the pull and write its journal in the run directory; the journal in use and the library stay as they are.
        #[arg(long)]
        dry_run: bool,
        /// Private run history and per-device journals (default: XDG_STATE_HOME/rustunes).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Maximum time for the device snapshot, and for copying each track.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
        seconds: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show or change this host's settings.
    Config {
        #[command(subcommand)]
        setting: ConfigCommand,
    },
    /// Update and verify the embedded cover for a track already verified in this journal.
    Artwork {
        file: PathBuf,
        /// Complete snapshot identifying the device and library; current state is captured again.
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        journal: PathBuf,
        #[arg(long)]
        trace_dir: PathBuf,
        #[arg(long, default_value = "rustunes")]
        host_name: String,
        /// Inspect the current device and report the exact artwork proposal without writes or reservations.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect local verified transfers and unfinished work without connecting to the phone.
    Status {
        /// Complete snapshot identifying the device and library of this journal.
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        journal: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Apply a saved plan through the same steps as sync, each pinned to the plan: the plan's removal first, if it has one (plan --remove-missing, --add-remove, --replace-changed, --replace-library), then a sync that must add and update exactly the plan's sources.
    Apply {
        plan: PathBuf,
        /// Complete snapshot used to create the plan.
        #[arg(long)]
        snapshot: PathBuf,
        /// Persistent journal directory; created privately if it does not yet exist.
        #[arg(long)]
        journal: PathBuf,
        /// New directory retaining each prestate, transfer and verification receipt.
        #[arg(long)]
        trace_dir: PathBuf,
        /// Human-readable sync host label advertised to the device.
        #[arg(long, default_value = "rustunes")]
        host_name: String,
        /// Reconstruct and validate the saved plan offline without creating a journal or writing to the phone.
        #[arg(long)]
        check_only: bool,
        /// Check the phone's current state against the saved plan and retain evidence; no phone writes or reservations.
        #[arg(long, conflicts_with = "check_only")]
        dry_run: bool,
        /// Confirm a plan's removals without an interactive prompt.
        #[arg(long)]
        yes: bool,
        /// New tracks, with their covers, per native session, as for sync.
        #[arg(long, default_value_t = DEFAULT_BATCH, value_parser = clap::value_parser!(u64).range(1..=MAX_BATCH))]
        batch_size: u64,
        /// Private run history (default: XDG_STATE_HOME/rustunes).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Maximum time for each fresh capture.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
        seconds: u64,
    },
    /// Plan sync against a complete saved device snapshot; no phone writes.
    Plan {
        /// The directory or file to plan (default: the library directory).
        path: Option<PathBuf>,
        /// Plan additions only and preserve existing tracks, retaining field differences in the report.
        #[arg(long)]
        add_only: bool,
        /// Plan each new FLAC as an ALAC converted from it (sync --flac-to-alac); FFmpeg is checked first.
        #[arg(long)]
        flac_to_alac: bool,
        /// Plan removing every track, then adding the selected sources again (sync --replace-library).
        #[arg(long, conflicts_with = "add_only")]
        replace_library: bool,
        /// Plan replacing the tracks this tool inserted whose source audio changed, or whose source cleared a genre or composer (sync --replace-changed).
        #[arg(long, conflicts_with_all = ["add_only", "replace_library"])]
        replace_changed: bool,
        /// Plan removing the tracks this tool inserted whose source file is gone from the selection, then the sync (as sync does).
        #[arg(long, conflicts_with_all = ["replace_library", "replace_changed"])]
        remove_missing: bool,
        /// Plan --add-only with --remove-missing.
        #[arg(long, conflicts_with_all = ["replace_library", "replace_changed"])]
        add_remove: bool,
        /// Complete snapshot directory created by snapshot; no device connection is made.
        #[arg(long)]
        snapshot: PathBuf,
        /// Existing local journal for ownership and reservation checks; never created by planning.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Save the complete JSON plan to a new private file.
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Plan the files in subfolders too; without it only the files directly in the directory are planned.
        #[arg(long)]
        recursive: bool,
        /// Include an exact root-relative file or subtree; repeat to select more.
        #[arg(long, value_name = "PATH")]
        include: Vec<String>,
        /// Exclude an exact root-relative file or subtree; exclusions win.
        #[arg(long, value_name = "PATH")]
        exclude: Vec<String>,
    },
    /// Save a complete read-only device catalogue and audio index for planning.
    Snapshot {
        /// New private snapshot directory; existing directories are never overwritten.
        directory: PathBuf,
        /// Whole capture deadline, including all audio readback.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=1800))]
        seconds: u64,
    },
    /// Inspect the current device library, track metadata and artwork registrations.
    Library {
        #[command(subcommand)]
        view: LibraryCommand,
        /// Print a JSON report to stdout.
        #[arg(long, global = true)]
        json: bool,
        /// Retain read-only capture evidence in a new private directory.
        #[arg(long, global = true)]
        trace_dir: Option<PathBuf>,
        /// Read a saved complete snapshot offline instead of connecting to the phone.
        #[arg(long, global = true, conflicts_with = "trace_dir")]
        snapshot: Option<PathBuf>,
    },
    /// Export the live Music catalog as CSV using read-only database/WAL copies.
    Catalog {
        /// New CSV file; omit to write CSV to stdout. Existing files are never overwritten.
        #[arg(long)]
        output: Option<PathBuf>,
        /// New private directory retaining capture evidence; temporary files are otherwise removed on success.
        #[arg(long)]
        trace_dir: Option<PathBuf>,
    },
    /// Inspect a local audio source (ALAC or AAC M4A, MP3, FLAC, WAV, AIFF) without connecting or modifying it.
    Inspect {
        file: PathBuf,
        /// Print everything the reader found as JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Validate metadata, audio container and artwork against this CLI's supported sync profile.
    Validate {
        file: PathBuf,
        /// Print a JSON report to stdout.
        #[arg(long)]
        json: bool,
        /// Fully decode the audio with FFmpeg (read-only, 120-second deadline).
        #[arg(long)]
        decode: bool,
    },
    /// Scan local audio sources, selections and duplicates without connecting to a phone.
    Scan {
        path: PathBuf,
        /// Report paths and exclusions without hashing audio or reading tags.
        #[arg(long)]
        selection_only: bool,
        /// Descend into the directories below PATH too.
        #[arg(long)]
        recursive: bool,
        /// Include an exact root-relative file or directory subtree; repeat for more paths.
        #[arg(long, value_name = "PATH")]
        include: Vec<String>,
        /// Exclude an exact root-relative file or subtree; exclusions win over inclusions.
        #[arg(long, value_name = "PATH")]
        exclude: Vec<String>,
        /// Print a JSON report to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Read device details using an existing pairing record. Never initiates pairing.
    #[command(hide = true)]
    DeviceInfo,
    /// Receive ATC startup messages; passive unless --ping is explicitly selected.
    Probe {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
        seconds: u64,
        /// Save received frames and any attempted Ping privately in this directory.
        #[arg(long)]
        trace_dir: PathBuf,
        /// Send exactly one reference Ping; no library synchronization is initiated.
        #[arg(long)]
        ping: bool,
    },
    /// Capture relevant device syslog lines without changing the device library.
    Syslog {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
        seconds: u64,
        /// Write selected logs privately to this new file.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Report entries, reuse and size.
    Stats {
        /// Count entries no index pass has used for this many days.
        #[arg(long, default_value_t = 90)]
        unseen_days: u64,
        #[arg(long)]
        json: bool,
    },
    /// Remove entries no index pass has used for this many days, such as deleted tracks.
    Prune {
        #[arg(long, default_value_t = 90)]
        unseen_days: u64,
        /// Count what would be removed without changing the cache.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LibraryCommand {
    /// List tracks, optionally filtering the current device catalogue.
    List {
        /// Case-insensitive substring search over title, artist and album.
        #[arg(long)]
        query: Option<String>,
        /// Exact artist or album-artist label.
        #[arg(long)]
        artist: Option<String>,
        /// Exact album label.
        #[arg(long)]
        album: Option<String>,
    },
    /// Show track, album, artist, size and artwork registration totals.
    Stats,
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// List USB identifiers, or with --wifi the paired phones answering on this
    /// network, without starting a phone service.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Read model and OS details using the existing pairing record.
    Info {
        #[arg(long)]
        json: bool,
        /// Print every value of this Lockdown domain instead, as an XML plist
        /// (for example com.apple.mobile.iTunes, which lists the audio codecs
        /// the phone accepts).
        #[arg(long)]
        domain: Option<String>,
    },
}

/// The fingerprint cache for every index pass in this run.
/// Without a resolvable state directory, passes copy every file as before.
pub(crate) fn fingerprint_cache(
    full_readback: bool,
) -> Option<crate::engine::state::audio_index::cache::Config> {
    use crate::engine::state::audio_index::cache::{Config, Mode};
    let directory = fingerprint_cache_directory().ok()?;
    Some(Config::new(
        directory,
        if full_readback {
            Mode::Refresh
        } else {
            Mode::Reuse
        },
    ))
}

fn fingerprint_cache_directory() -> Result<PathBuf> {
    Ok(default_state_directory()?.join("fingerprint-cache"))
}

/// The sources a command plans from, read ahead while a Wi-Fi phone is found.
fn sources_to_warm(command: &Command) -> Option<(PathBuf, source_collection::SelectionOptions)> {
    match command {
        Command::Sync { recursive, .. } => Some((
            config::library().ok()?,
            source_collection::SelectionOptions {
                recursive: *recursive,
                ..Default::default()
            },
        )),
        Command::Push {
            path,
            recursive,
            include,
            exclude,
            ..
        } => Some((
            path.clone().or_else(|| config::library().ok())?,
            source_collection::SelectionOptions {
                recursive: *recursive,
                include: include.iter().cloned().collect(),
                exclude: exclude.iter().cloned().collect(),
            },
        )),
        _ => None,
    }
}

/// Bonjour, waking the phone, and holding it awake (taken again if it was
/// already falling asleep).
const WIFI_DISCOVERY: Duration = Duration::from_secs(90);

/// The largest `--batch-size`.
const MAX_BATCH: u64 = crate::engine::planning::change::MAX_BATCH as u64;
/// A session's operations by default: all one operation file holds, as
/// sessions of 499 insertions and of 499 removals were verified on the phone.
const DEFAULT_BATCH: u64 = crate::engine::planning::change::DEFAULT_BATCH as u64;

/// USB unless --wifi or --wifi-address chose Wi-Fi.
fn device_config(cli: &Cli) -> Result<engine::DeviceConfig> {
    let config = usb_config(cli.device.clone())?;
    Ok(if cli.wifi || cli.wifi_address.is_some() {
        config.over_wifi(cli.wifi_address, wifi_memory())
    } else {
        config
    })
}
/// Where the phone's last Wi-Fi address is remembered.
pub(crate) fn wifi_memory() -> Option<PathBuf> {
    default_state_directory()
        .ok()
        .map(|state| state.join("wifi"))
}
/// Where a run locks each phone it reaches, whatever `--state-dir` says, so
/// that runs with different state directories also take turns.
fn phone_locks() -> Option<PathBuf> {
    default_state_directory()
        .ok()
        .map(|state| state.join("locks"))
}
fn usb_config(selected: Option<String>) -> Result<engine::DeviceConfig> {
    Ok(
        engine::DeviceConfig::new(selected, std::env::var_os("USBMUXD_SOCKET_ADDRESS"))?
            .locked_in(phone_locks()),
    )
}

/// The plan's actions, counts and blockers, as a `plan` report: what the
/// terminal shows of it. The complete plan is the file at `plan_file`.
pub(crate) fn plan_report(result: &engine::PlanResult) -> serde_json::Value {
    use serde_json::json;
    let plan = &result.plan;
    let mut actions = Vec::new();
    for source in &plan.sources {
        let path = &source.relative_path;
        match &source.decision {
            additive::Decision::Add {
                track_id,
                path: destination,
                ..
            } => actions.push(json!({
                "action": "add",
                "path": path,
                "track_id": track_id,
                "destination": destination,
                "flac_to_alac": source.flac_to_alac,
            })),
            additive::Decision::Conflict { code, message } => actions.push(json!({
                "action": "conflict",
                "path": path,
                "code": code,
                "reason": additive::skip::reason(source, code, message, &plan.sources),
                "message": message,
            })),
            additive::Decision::Skip {
                code,
                reason,
                message,
            } => actions.push(json!({
                "action": "skip",
                "path": path,
                "code": code,
                "reason": reason,
                "detail": message,
            })),
            additive::Decision::Update { track_id, changes } => actions.push(json!({
                "action": "update",
                "path": path,
                "track_id": track_id,
                "changes": changes.iter().map(|change| json!({
                    "field": change.field,
                    "current": change.current,
                    "desired": change.desired,
                })).collect::<Vec<_>>(),
            })),
            additive::Decision::Keep => {
                if let additive::ArtworkDecision::PendingOwnedCover { bytes, .. } = source.artwork {
                    actions.push(json!({"action": "finish_cover", "path": path, "bytes": bytes}));
                }
            }
        }
        if let Some(bytes) = source.cover_resent(result.add_only) {
            actions.push(json!({"action": "send_cover", "path": path, "bytes": bytes}));
        }
    }
    json!({
        "ready": result.ready(),
        "add_only": result.add_only,
        "additions": result.additions,
        "matched": result.matched,
        "updates": result.updates,
        "conflicts": result.conflicts,
        "skipped": result.skipped,
        "deletions": 0,
        "snapshot_revision": plan.media_anchor,
        "actions": actions,
        "blockers": plan.journal_blockers,
        "artwork_recent_resets": {
            "plays": plan.artwork_import_recent_play_resets,
            "skips": plan.artwork_import_recent_skip_resets,
        },
        "preserved_unmatched_device_tracks": plan.preserved_unmatched_device_tracks.len(),
        "excluded_sources": result.source_exclusions,
    })
}

/// The `catalog` columns, as a `catalog` report.
fn catalog_json(catalog: &music_protocol::catalogue::sqlite::Catalog) -> serde_json::Value {
    let tracks: Vec<_> = catalog
        .tracks()
        .iter()
        .map(|track| {
            let meta = &track.metadata;
            serde_json::json!({
                "wire_id": (track.sync_id as i64).to_string(),
                "title": meta.title,
                "artist": meta.artist,
                "album": meta.album,
                "album_artist": meta.album_artist,
                "track_number": meta.track_number,
                "track_count": meta.track_count,
                "disc_number": meta.disc_number,
                "disc_count": meta.disc_count,
                "year": meta.year,
                "duration_ms": meta.total_time_ms,
                "size_bytes": meta.file_size,
                "path": track.afc_path,
            })
        })
        .collect();
    serde_json::json!({"track_count": tracks.len(), "tracks": tracks})
}

/// A property list value as JSON; data is base64, a date its XML text.
fn plist_json(value: &plist::Value) -> serde_json::Value {
    use plist::Value;
    match value {
        Value::Array(items) => items.iter().map(plist_json).collect(),
        Value::Dictionary(entries) => entries
            .iter()
            .map(|(key, value)| (key.clone(), plist_json(value)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        Value::Boolean(value) => (*value).into(),
        Value::Data(bytes) => serde_json::json!({"base64": base64(bytes)}),
        Value::Date(date) => date.to_xml_format().into(),
        Value::Real(value) => (*value).into(),
        Value::Integer(value) => value
            .as_signed()
            .map(serde_json::Value::from)
            .or_else(|| value.as_unsigned().map(serde_json::Value::from))
            .unwrap_or_default(),
        Value::String(text) => text.as_str().into(),
        Value::Uid(uid) => uid.get().into(),
        _ => serde_json::Value::Null,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | u32::from(*byte) << (16 - 8 * i));
        for i in 0..4 {
            text.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    text
}

/// The plan as the terminal shows it, or its JSON document if `json`.
pub(crate) fn write_plan(
    out: &mut impl Write,
    result: &engine::PlanResult,
    json: bool,
    add_only: bool,
) -> Result<()> {
    let plan = &result.plan;
    let bytes = &result.serialized;
    let adds = result.additions;
    let keeps = result.matched;
    let updates = result.updates;
    let conflicts = result.conflicts;
    // Only a plan that skips says so: the line is otherwise unchanged.
    let skipped = match result.skipped {
        0 => String::new(),
        n => format!(", {n} skipped"),
    };
    if json {
        out.write_all(bytes)?;
    } else {
        if add_only {
            writeln!(
                out,
                "Add-only policy: preserve existing audio and metadata; finish only this tool's pending covers."
            )?;
        }
        writeln!(
            out,
            "Additive plan: {adds} additions, {keeps} matched, {updates} updates, {conflicts} conflicts{skipped}, 0 deletions"
        )?;
        write_artwork_recent_disclosure(out, plan)?;
        writeln!(
            out,
            "Saved snapshot revision {}; execution requires fresh device checks.",
            plan.media_anchor
        )?;
        for source in &plan.sources {
            match &source.decision {
                additive::Decision::Add { track_id, path, .. } => {
                    let converted = if source.flac_to_alac {
                        " (FLAC → ALAC)"
                    } else {
                        ""
                    };
                    writeln!(
                        out,
                        "ADD {:?} → {track_id} at {path}{converted}",
                        source.relative_path
                    )?;
                    for issue in source
                        .validation_issues
                        .iter()
                        .filter(|issue| issue.code == "cover_left_out")
                    {
                        writeln!(out, "  {}", issue.message)?;
                    }
                }
                additive::Decision::Conflict { code, message } => {
                    writeln!(
                        out,
                        "CONFLICT {:?}: {}",
                        source.relative_path,
                        additive::skip::reason(source, code, message, &plan.sources)
                    )?;
                }
                additive::Decision::Skip { reason, .. } => {
                    writeln!(out, "SKIP {:?}: {reason}", source.relative_path)?;
                }
                additive::Decision::Update { track_id, changes } => {
                    writeln!(out, "UPDATE {:?} → {track_id}", source.relative_path)?;
                    for change in changes {
                        writeln!(
                            out,
                            "  {}: {} → {}",
                            change.field, change.current, change.desired
                        )?;
                    }
                }
                additive::Decision::Keep => {
                    if source.mp3.is_some() {
                        write_mpeg_keep(out, &source.relative_path, &source.matches)?;
                    }
                    if let additive::ArtworkDecision::PendingOwnedCover { bytes, .. } =
                        source.artwork
                    {
                        writeln!(
                            out,
                            "COVER {:?}: finish pending cover ({bytes} bytes)",
                            source.relative_path
                        )?;
                    }
                }
            }
            if let Some(bytes) = source.cover_resent(add_only) {
                writeln!(
                    out,
                    "COVER {:?}: send the cover again ({bytes} bytes); no journal record proves the phone's",
                    source.relative_path
                )?;
            }
        }
        let resent = plan
            .sources
            .iter()
            .filter(|source| source.cover_resent(add_only).is_some())
            .count();
        if resent > 0 {
            writeln!(
                out,
                "Covers sent again: {resent}, one phone session each; `rustunes pull` first keeps those it reads back as the expected image."
            )?;
        }
        for blocker in &plan.journal_blockers {
            writeln!(out, "BLOCKED: {blocker}")?;
        }
        writeln!(
            out,
            "Preserve {} unmatched device tracks; excluded {} source entries.",
            plan.preserved_unmatched_device_tracks.len(),
            result.source_exclusions
        )?;
        writeln!(
            out,
            "Artwork status and complete field differences are included in --json or --output."
        )?;
    }
    Ok(())
}

pub(crate) fn write_mpeg_keep(
    out: &mut impl Write,
    relative_path: &str,
    matches: &[additive::ExistingMatch],
) -> std::io::Result<()> {
    let (file, ownership) = match matches {
        [matched] if matched.match_basis == Some("owned_mpeg_frame_identity") => (
            "the file this tool sent (same encoded audio)",
            matched.ownership,
        ),
        [matched] if matched.match_basis == Some("flac_conversion") => {
            ("the ALAC this tool converted from it", matched.ownership)
        }
        [matched] => ("exact existing audio file", matched.ownership),
        _ => ("exact existing audio file", "not established"),
    };
    writeln!(
        out,
        "KEEP {relative_path:?}: {file}; ownership: {ownership}; catalogue metadata and artwork preserved."
    )
}

async fn connected_provider() -> Result<device::DeviceProvider> {
    timeout(Duration::from_secs(15), device::provider())
        .await
        .context("timed out discovering the device")?
}

/// Whether a syslog capture keeps this line: one from a music process (Music,
/// musicd, medialibraryd) or the sync stack (ATC, AirTraffic, ATAirlock).
fn relevant_syslog(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    // Avoid Watch/batch and unrelated camera settings that list the Music app.
    let prefix = lower.split('<').next().unwrap_or(&lower);
    let music_process = prefix
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| matches!(word, "music" | "musicd" | "medialibraryd"));
    music_process
        || lower
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| word == "atc")
        || lower.contains("airtraffic")
        || lower.contains("atairlock")
}

pub(crate) fn write_artwork_recent_disclosure(
    out: &mut impl std::io::Write,
    plan: &additive::Plan,
) -> Result<()> {
    if !plan.artwork_import_recent_skip_resets.is_empty() {
        writeln!(
            out,
            "Artwork import may reset recent-play counters on {} tracks and recent-skip counters on {} tracks; verification requires both recent counters unchanged or reset all to zero. Lifetime counts, dates and all other statistics must stay unchanged.",
            plan.artwork_import_recent_play_resets.len(),
            plan.artwork_import_recent_skip_resets.len()
        )?;
        for (kind, resets) in [
            ("PLAY", &plan.artwork_import_recent_play_resets),
            ("SKIP", &plan.artwork_import_recent_skip_resets),
        ] {
            for reset in resets {
                writeln!(
                    out,
                    "RECENT {kind} {}: {} -> {}",
                    reset["track_id"], reset["before"], reset["after"]
                )?;
            }
        }
    } else if !plan.artwork_import_recent_play_resets.is_empty() {
        writeln!(
            out,
            "Artwork import may reset recent-play counters on {} tracks; lifetime counts must stay unchanged.",
            plan.artwork_import_recent_play_resets.len()
        )?;
        for reset in &plan.artwork_import_recent_play_resets {
            writeln!(
                out,
                "RECENT PLAY {}: {} -> {}",
                reset["track_id"], reset["before"], reset["after"]
            )?;
        }
    }
    Ok(())
}

fn default_state_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        anyhow::ensure!(path.is_absolute(), "XDG_STATE_HOME must be absolute");
        return Ok(path.join("rustunes"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("set --state-dir or HOME")?);
    anyhow::ensure!(home.is_absolute(), "HOME must be absolute; use --state-dir");
    Ok(home.join(".local/state/rustunes"))
}
