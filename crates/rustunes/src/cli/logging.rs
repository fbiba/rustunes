//! The log of each run, for bug reports: a private file in the state
//! directory, and with `--verbose` the same lines on stderr. It holds the
//! build and system, the command line, the lines the terminal shows (without
//! the per-chunk progress), the phone's model and iOS version, the steps that
//! fail quietly, the error a command ends with and a panic's backtrace, and
//! what the libraries that reach the phone log at `info` and above.
//!
//! A run that fails, with an error or a panic, keeps its log in
//! `logs/failed` as well, where later runs do not rotate it away, and prints
//! the last lines of it the terminal did not show, then where it is.
use super::{deletion, machine, progress};
use crate::engine::{Event, Observation, execution::delete, state::layout};
use music_protocol::protocol::trace::private_file;

use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{filter::Targets, fmt, prelude::*};

/// Logs kept in the state directory's `logs`, this run's included, and in
/// `logs/failed`.
const KEPT: usize = 20;

/// The lines of the log a failure prints: the last ones the terminal did not
/// show.
const RECENT: usize = 20;

/// What a run logs unless `-vv`, `-vvv` or `RUSTUNES_LOG` asks for more.
const DEFAULT: &str = "info,rustunes=debug";

/// The target of what the terminal shows already, on stderr or as records on
/// stdout, which `--verbose` does not print again: lines, the error a command
/// fails with, and a panic (with its backtrace, in the file).
const SHOWN: &str = "rustunes::shown";

static PATH: OnceLock<PathBuf> = OnceLock::new();
/// This run's log in `logs/failed`, once it failed.
static FAILED: OnceLock<Option<PathBuf>> = OnceLock::new();
/// The last [`RECENT`] lines of the log the terminal did not show, unless
/// `--verbose` printed them.
static RECENT_LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Start this run's log. Only an unreadable `RUSTUNES_LOG` fails; without a
/// usable state directory the run goes without a file. `verbose` counts `-v`.
pub(super) fn start(verbose: u8) -> Result<()> {
    let filter = filter(verbose, std::env::var("RUSTUNES_LOG"))?;
    let (file, unopened) = match open() {
        Ok((path, file)) => {
            let _ = PATH.set(path);
            let layer = fmt::layer()
                .with_writer(Arc::new(file))
                .with_ansi(false)
                .with_filter(filter.clone());
            (Some(layer), None)
        }
        Err(error) => (None, Some(error)),
    };
    let unshown = filter.with_target(SHOWN, LevelFilter::OFF);
    let (stderr, recent) = if verbose > 0 {
        let layer = fmt::layer()
            .with_writer(io::stderr)
            .with_ansi(false)
            .with_filter(unshown);
        (Some(layer), None)
    } else {
        let layer = fmt::layer()
            .with_writer(|| Recent(Vec::new()))
            .with_ansi(false)
            .without_time()
            .with_filter(unshown);
        (None, Some(layer))
    };
    let _ = tracing_subscriber::registry()
        .with(file)
        .with(stderr)
        .with(recent)
        .try_init();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(target: SHOWN, "{info}\n{backtrace}");
        keep();
        default(info);
        dump();
    }));
    if let Some(error) = unopened {
        tracing::warn!("This run has no log file: {error:#}");
    }
    tracing::info!(target: "rustunes", "{}", platform());
    let args: Vec<_> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    tracing::info!(target: "rustunes", "Command line: {args:?}");
    Ok(())
}

/// One line of the log, added to [`RECENT_LINES`] once it is written.
struct Recent(Vec<u8>);
impl io::Write for Recent {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Recent {
    fn drop(&mut self) {
        let line = String::from_utf8_lossy(&self.0).trim_end().to_owned();
        if line.is_empty() {
            return;
        }
        let mut recent = RECENT_LINES.lock().unwrap_or_else(|e| e.into_inner());
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(line);
    }
}

/// `RUSTUNES_LOG` (`variable`), else what `-v` asks for: the default once,
/// the libraries' debug output twice (but not AFC's, which prints every byte
/// it reads), and everything three times.
fn filter(verbose: u8, variable: Result<String, std::env::VarError>) -> Result<Targets> {
    let asked = match verbose {
        0 | 1 => DEFAULT,
        2 => "debug,idevice::services::afc=info",
        _ => "trace",
    };
    let explain =
        "RUSTUNES_LOG must be a log filter: a comma-separated list of `level` or `target=level`";
    match variable {
        Ok(value) if !value.is_empty() => value.parse().context(explain),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{explain}, in UTF-8"),
        _ => Ok(asked.parse().expect("the built-in filters parse")),
    }
}

/// A new private log in `state/logs`, once all but the newest earlier ones
/// are removed. It is named as runs are: nanoseconds since the epoch, then
/// the process ID.
fn open() -> Result<(PathBuf, fs::File)> {
    let state = super::default_state_directory()?;
    layout::private_directory(&state)?;
    let logs = state.join("logs");
    layout::private_directory(&logs)?;
    prune(&logs);
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = logs.join(format!("{timestamp}-{}.log", std::process::id()));
    let file = private_file(&path)?;
    Ok((path, file))
}

/// Remove the oldest logs, leaving room for this run's among the [`KEPT`].
/// Files this module did not name are left alone.
fn prune(logs: &Path) {
    let Ok(entries) = fs::read_dir(logs) else {
        return;
    };
    let mut named: Vec<(u128, u32, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let (timestamp, process) = name.to_str()?.strip_suffix(".log")?.split_once('-')?;
            let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            if !(digits(timestamp) && digits(process) && entry.file_type().ok()?.is_file()) {
                return None;
            }
            Some((timestamp.parse().ok()?, process.parse().ok()?, entry.path()))
        })
        .collect();
    named.sort();
    let excess = (named.len() + 1).saturating_sub(KEPT);
    for (.., path) in &named[..excess] {
        let _ = fs::remove_file(path);
    }
}

/// This build and the system it runs on.
fn platform() -> String {
    let distribution = fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|release| {
            release.lines().find_map(|line| {
                Some(
                    line.strip_prefix("PRETTY_NAME=")?
                        .trim_matches('"')
                        .to_owned(),
                )
            })
        })
        .unwrap_or_else(|| "Linux".into());
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    format!(
        "rustunes {} on {distribution} (kernel {}, {})",
        env!("CARGO_PKG_VERSION"),
        kernel.trim(),
        std::env::consts::ARCH
    )
}

/// This run's log, if the state directory could hold it.
pub(super) fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// Keep this failed run's log in `logs/failed` too, linked under the same
/// name once the oldest there beyond [`KEPT`] are removed. Best effort, and
/// once; the log in `logs` stays where the `start` record names it.
fn keep() -> Option<&'static Path> {
    FAILED
        .get_or_init(|| {
            let path = path()?;
            let failed = path.parent()?.join("failed");
            layout::private_directory(&failed).ok()?;
            prune(&failed);
            let kept = failed.join(path.file_name()?);
            fs::hard_link(path, &kept).ok()?;
            Some(kept)
        })
        .as_deref()
}

/// After a failure, outside `--message-format json`: the last lines of the
/// log the terminal did not show, then where the log is kept. `--verbose`
/// printed those lines already, and the `start` record names the log.
pub(super) fn dump() {
    if machine::enabled() {
        return;
    }
    let recent = std::mem::take(&mut *RECENT_LINES.lock().unwrap_or_else(|e| e.into_inner()));
    if !recent.is_empty() {
        eprintln!("Last lines of the log not shown above:");
        for line in recent {
            eprintln!("  {line}");
        }
    }
    if let Some(path) = keep().or(path()) {
        eprintln!("Log of this run: {}", path.display());
    }
}

/// The command's end: the error it failed with, which keeps the log, or its
/// exit code.
pub(super) fn finished(result: &Result<ExitCode>) {
    match result {
        Err(error) => {
            tracing::error!(target: SHOWN, "{error:?}");
            keep();
        }
        Ok(_) => tracing::info!("Finished with exit code {}", machine::exit_code(result)),
    }
}

/// A notice's line, as the terminal shows it.
pub(super) fn notice(level: machine::Level, line: &str) {
    match level {
        machine::Level::Info => tracing::info!(target: SHOWN, "{line}"),
        machine::Level::Warning => tracing::warn!(target: SHOWN, "{line}"),
    }
}

/// An engine event's line, as the terminal shows it. Results are logged as
/// the lines they print on stdout; a plan and a removal's selection are not,
/// as the run directory and the evidence keep them.
pub(super) fn event(event: &Event<'_>) {
    if floods(event) {
        return;
    }
    let line = match event {
        Event::SyncFinished(result) => Some(progress::finished(result).join("\n")),
        Event::ApplyFinished(result) => Some(progress::applied(result)),
        Event::CoverFinished(result) => Some(progress::covered(result)),
        Event::Deletion(delete::Event::Completed {
            removed,
            deleted_playlists,
            left,
            evidence,
        }) => Some(deletion::completed_line(
            *removed,
            *deleted_playlists,
            *left,
            *evidence,
        )),
        Event::Deletion(event) => deletion::line(event),
        other => progress::line(other).ok().flatten(),
    };
    if let Some(line) = line.filter(|line| !line.is_empty()) {
        tracing::info!(target: SHOWN, "{line}");
    }
}

/// Progress reported once per chunk, and the per-track lines of a cached
/// pass, which would flood the log.
fn floods(event: &Event<'_>) -> bool {
    progress::thinned(event)
        || matches!(
            event,
            Event::Observation(
                Observation::CopyingTrackAudio { .. } | Observation::InspectingSource { .. }
            )
        )
}

#[cfg(test)]
mod tests;
