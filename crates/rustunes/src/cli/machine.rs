//! `--message-format json`: the command's output as one JSON object per line
//! on stdout, for programs that drive rustunes, such as a GUI. Nothing else is
//! written to stdout, and rustunes writes nothing of its own to stderr.
//!
//! Every record names its `type` and carries `seq` (0, 1, 2, ... with no gaps)
//! and `elapsed_ms` since the command started. The first record is `start`
//! and the last is `result`; between them come `phase`, `progress`, `notice`,
//! `confirm` and `report` records. docs/07-sync-engine-design.md, section 10, describes each.
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    process::ExitCode,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Names the record format; a change that could break a reader renames it.
pub(super) const SCHEMA: &str = "rustunes.messages";

/// A progress stream is written at most this often, apart from the first
/// record of each pass and the one that completes it.
const INTERVAL: Duration = Duration::from_millis(250);

/// An estimate needs this much of a stream behind it.
const SETTLED: Duration = Duration::from_secs(1);

static SINK: OnceLock<Sink> = OnceLock::new();

struct Sink {
    started: Instant,
    state: Mutex<State>,
    out: Mutex<Box<dyn Write + Send>>,
}

#[derive(Default)]
struct State {
    seq: u64,
    phase: Option<&'static str>,
    streams: BTreeMap<&'static str, Stream>,
    outcome: Option<&'static str>,
}

/// One progress stream since its pass began: a pass begins again when its
/// totals change or its counts go back.
struct Stream {
    began: Instant,
    /// Counts when the pass began, and as last reported.
    base: (u64, u64),
    last: (u64, u64),
    total: Option<u64>,
    bytes_total: Option<u64>,
    written: Option<Instant>,
}

/// Whether `--message-format json` was given.
pub(crate) fn enabled() -> bool {
    SINK.get().is_some()
}

/// Write records to stdout from now on, starting with `start`, which names
/// this run's `log` when it has one.
pub(super) fn enable(command: &str, options: Value, log: Option<&std::path::Path>) -> Result<()> {
    install(Box::new(std::io::stdout()))?;
    let mut fields = json!({
        "schema": SCHEMA,
        "version": env!("CARGO_PKG_VERSION"),
        "command": command,
        "options": options,
        "pid": std::process::id(),
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    });
    if let Some(log) = log {
        fields["log"] = log.to_string_lossy().into_owned().into();
    }
    write("start", fields)
}

fn install(out: Box<dyn Write + Send>) -> Result<()> {
    let sink = Sink {
        started: Instant::now(),
        state: Mutex::default(),
        out: Mutex::new(out),
    };
    SINK.set(sink)
        .map_err(|_| anyhow::anyhow!("machine output already enabled"))
}

/// Write one record. `fields` is an object; its keys follow the envelope.
fn write(kind: &str, fields: Value) -> Result<()> {
    let sink = SINK.get().context("machine output is not enabled")?;
    let mut state = sink.state.lock().unwrap_or_else(|e| e.into_inner());
    write_locked(sink, &mut state, kind, fields)
}

fn write_locked(sink: &Sink, state: &mut State, kind: &str, fields: Value) -> Result<()> {
    let line = record(kind, state.seq, sink.started.elapsed().as_millis(), fields)?;
    state.seq += 1;
    let mut out = sink.out.lock().unwrap_or_else(|e| e.into_inner());
    out.write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .context("writing a --message-format json record to stdout")
}

/// One record's line: the envelope, then `fields` (an object) in key order.
fn record(kind: &str, seq: u64, elapsed_ms: u128, fields: Value) -> Result<String> {
    let mut line = format!(
        "{{\"type\":{},\"seq\":{seq},\"elapsed_ms\":{elapsed_ms}",
        serde_json::to_string(kind)?
    );
    if let Value::Object(fields) = fields {
        for (key, value) in fields {
            line.push(',');
            line.push_str(&serde_json::to_string(&key)?);
            line.push(':');
            line.push_str(&serde_json::to_string(&value)?);
        }
    }
    line.push_str("}\n");
    Ok(line)
}

/// Enter `phase`, unless the command is already in it.
pub(super) fn phase(phase: &'static str, message: Option<String>) -> Result<()> {
    let sink = SINK.get().context("machine output is not enabled")?;
    let mut state = sink.state.lock().unwrap_or_else(|e| e.into_inner());
    enter(sink, &mut state, phase, message)
}

fn enter(
    sink: &Sink,
    state: &mut State,
    phase: &'static str,
    message: Option<String>,
) -> Result<()> {
    if state.phase == Some(phase) {
        return Ok(());
    }
    state.phase = Some(phase);
    let mut fields = json!({ "phase": phase });
    if let Some(message) = message {
        fields["message"] = message.into();
    }
    write_locked(sink, state, "phase", fields)
}

#[derive(Clone, Copy)]
pub(super) enum Level {
    Info,
    Warning,
}

/// The command's own result data, as `--json` prints it where it has one.
pub(super) fn report(kind: &'static str, report: &impl serde::Serialize) -> Result<()> {
    write(
        "report",
        json!({ "kind": kind, "report": serde_json::to_value(report)? }),
    )
}

/// `report`, for a renderer that writes its JSON to a writer.
pub(super) fn report_from(
    kind: &'static str,
    render: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut bytes = Vec::new();
    render(&mut bytes)?;
    report_bytes(kind, &bytes)
}

/// `report`, for a report already serialized as JSON.
pub(super) fn report_bytes(kind: &'static str, bytes: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(bytes).context("reading a JSON report")?;
    report(kind, &value)
}

/// What the command ended with, for its `result` record; a later call wins.
pub(super) fn outcome(outcome: &'static str) {
    if let Some(sink) = SINK.get() {
        sink.state.lock().unwrap_or_else(|e| e.into_inner()).outcome = Some(outcome);
    }
}

/// One update of a progress stream.
pub(super) struct Progress {
    phase: &'static str,
    unit: &'static str,
    done: u64,
    total: Option<u64>,
    bytes: Option<(u64, Option<u64>)>,
    current: Option<Value>,
    message: Option<String>,
}

impl Progress {
    /// `done` of `total` `unit`s in `phase`, which names the stream.
    pub(super) fn new(
        phase: &'static str,
        unit: &'static str,
        done: usize,
        total: Option<usize>,
    ) -> Self {
        Self {
            phase,
            unit,
            done: done as u64,
            total: total.map(|t| t as u64),
            bytes: None,
            current: None,
            message: None,
        }
    }
    /// Bytes done of `total`; estimates then follow bytes rather than items.
    pub(super) fn bytes(mut self, done: u64, total: Option<u64>) -> Self {
        self.bytes = Some((done, total));
        self
    }
    /// The item in hand: an object naming a file or track.
    pub(super) fn current(mut self, current: Value) -> Self {
        self.current = Some(current);
        self
    }
    pub(super) fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

/// Write a progress record, entering its phase first. Updates closer together
/// than [`INTERVAL`] are dropped, except as that constant says.
pub(super) fn progress(update: Progress) -> Result<()> {
    let sink = SINK.get().context("machine output is not enabled")?;
    let mut state = sink.state.lock().unwrap_or_else(|e| e.into_inner());
    let phase = update.phase;
    let Some(fields) = state.advance(update, Instant::now()) else {
        return Ok(());
    };
    enter(sink, &mut state, phase, None)?;
    write_locked(sink, &mut state, "progress", fields)
}

impl State {
    /// Take `update` into its stream at `now`: the progress record's fields,
    /// or `None` when it is too soon after the last.
    fn advance(&mut self, update: Progress, now: Instant) -> Option<Value> {
        // An update continues its stream's pass while the total stays and the
        // count does not go back; a new pass inherits nothing.
        let previous = self
            .streams
            .get(update.phase)
            .filter(|stream| stream.total == update.total && update.done >= stream.last.0);
        let counted = update.total.is_some_and(|t| update.done >= t);
        // An update without a byte total keeps its stream's; one with a count
        // alone keeps the bytes the stream last had, all of them once the
        // count is complete.
        let (bytes, bytes_total) = match (update.bytes, previous) {
            (Some((done, Some(total))), _) => (Some(done), Some(total)),
            (Some((done, None)), previous) => (Some(done), previous.and_then(|s| s.bytes_total)),
            (None, Some(stream)) if stream.bytes_total.is_some() => (
                Some(if counted {
                    stream.bytes_total.unwrap_or(stream.last.1)
                } else {
                    stream.last.1
                }),
                stream.bytes_total,
            ),
            (None, _) => (None, None),
        };
        // A pass may gain a byte total, written at once, but a different one
        // starts another.
        let gained =
            previous.is_some_and(|stream| stream.bytes_total.is_none()) && bytes_total.is_some();
        let restart = previous.is_none_or(|stream| {
            stream
                .bytes_total
                .is_some_and(|total| bytes_total != Some(total))
                || bytes.is_some_and(|b| b < stream.last.1)
        });
        let counts = (update.done, bytes.unwrap_or(0));
        let complete = counted || bytes_total.is_some_and(|t| t > 0 && counts.1 >= t);
        // A completed pass is written once, however many updates repeat it.
        if !restart && complete && previous.is_some_and(|stream| stream.last == counts) {
            return None;
        }
        if restart {
            self.streams.insert(
                update.phase,
                Stream {
                    began: now,
                    base: counts,
                    last: counts,
                    total: update.total,
                    bytes_total,
                    written: None,
                },
            );
        }
        let stream = self.streams.get_mut(update.phase)?;
        stream.last = counts;
        stream.bytes_total = bytes_total;
        let due = stream
            .written
            .is_none_or(|written| now.duration_since(written) >= INTERVAL);
        if !(due || complete || gained) {
            return None;
        }
        stream.written = Some(now);
        // Without bytes of its own, a stream counts the current item's part.
        let partial = match (&update.current, bytes) {
            (Some(current), None) => match (
                current["bytes_done"].as_u64(),
                current["bytes_total"].as_u64(),
            ) {
                (Some(done), Some(total)) if total > 0 && done < total => {
                    done as f64 / total as f64
                }
                _ => 0.0,
            },
            _ => 0.0,
        };
        let estimate = Estimate::of(
            stream,
            now,
            update.done as f64 + partial,
            update.total,
            bytes,
            bytes_total,
        );
        let mut fields = json!({
            "phase": update.phase,
            "unit": update.unit,
            "done": update.done,
            "total": update.total,
            "percent": estimate.percent,
            "eta_ms": estimate.eta_ms,
            "rate_per_second": estimate.rate,
        });
        if let Some(bytes) = bytes {
            fields["bytes_done"] = bytes.into();
            fields["bytes_total"] = bytes_total.into();
            fields["bytes_per_second"] = estimate.byte_rate.into();
        }
        if let Some(current) = update.current {
            fields["current"] = current;
        }
        if let Some(message) = update.message {
            fields["message"] = message.into();
        }
        Some(fields)
    }
}

/// How far a stream has come and, from its average rate since its pass
/// began, how long the rest should take.
#[derive(Debug, PartialEq)]
struct Estimate {
    percent: Option<f64>,
    eta_ms: Option<u64>,
    rate: Option<f64>,
    byte_rate: Option<f64>,
}

impl Estimate {
    /// `done` items, with the current item's part.
    fn of(
        stream: &Stream,
        now: Instant,
        done: f64,
        total: Option<u64>,
        bytes: Option<u64>,
        bytes_total: Option<u64>,
    ) -> Self {
        let seconds = now.duration_since(stream.began).as_secs_f64();
        let settled = now.duration_since(stream.began) >= SETTLED;
        let per_second =
            |progressed: f64| (settled && progressed > 0.0).then(|| round(progressed / seconds, 3));
        let rate = per_second(done - stream.base.0 as f64);
        let byte_rate = bytes.and_then(|b| per_second(b.saturating_sub(stream.base.1) as f64));
        // Bytes say more than items when the stream counts them.
        let (left, speed, fraction) = match (bytes, bytes_total) {
            (Some(b), Some(t)) if t > 0 => {
                (t.saturating_sub(b) as f64, byte_rate, b as f64 / t as f64)
            }
            _ => match total {
                Some(t) if t > 0 => ((t as f64 - done).max(0.0), rate, done / t as f64),
                Some(_) => (0.0, rate, 1.0),
                None => {
                    return Self {
                        percent: None,
                        eta_ms: None,
                        rate,
                        byte_rate,
                    };
                }
            },
        };
        Self {
            percent: Some(round(100.0 * fraction.min(1.0), 1)),
            eta_ms: if left <= 0.0 {
                Some(0)
            } else {
                speed.map(|speed| (left / speed * 1000.0) as u64)
            },
            rate,
            byte_rate,
        }
    }
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}
