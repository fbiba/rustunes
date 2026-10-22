//! FLACs converted ahead of their insert batches, as Apple's host converts
//! ahead of its upload: a few conversions at once, in member order, through
//! the current batch and on into the next one while the current one is sent.
//!
//! A batch begins once the conversion directory has room for twice the size
//! of its FLACs not yet converting, beside the conversions under way. A FLAC
//! of the next batch starts converting only while there is room for twice its
//! size beside those. A batch then takes its members' conversions in member
//! order, so it refuses the same FLACs in the same order as converting just
//! before it would. Stopping (an error, a cancel, the run's end) kills FFmpeg
//! and removes every conversion no batch took.
use anyhow::{Result, bail, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::mpsc;

use super::{Audio, Member};
use crate::{
    engine::execution::flac_to_alac::{self, Converted},
    engine::planning::source_collection::mp3::preparation::PreparedSource,
};

/// At most this many conversions run at once, and never more than the
/// processors but one, as Apple's host defaults its transcoding threads.
const THREADS: usize = 3;
/// At most this many FLACs go in one batch, and convert ahead of the next:
/// their conversions take up to twice their size in `TMPDIR`, often RAM,
/// about 4 GB for FLACs of 20 MB. Larger batches would gain little, as each
/// session's own checks take about 12.5 s beside its uploads.
const FLACS: usize = 100;
/// How often a conversion waiting for room looks again, and a batch waiting
/// for its conversions looks for a cancel.
const POLL: Duration = Duration::from_millis(250);

/// One FLAC to convert: its member's index in the run.
pub(super) struct Job {
    pub(super) index: usize,
    pub(super) flac: PathBuf,
    pub(super) prepared: Arc<PreparedSource>,
    /// The planned cover's SHA-256: the ALAC must hold the same.
    cover: Option<[u8; 32]>,
    /// The FLAC's size; converting it needs room for twice as much.
    bytes: u64,
}

/// What became of a job.
enum Outcome {
    Converted(Box<Converted>),
    Refused(anyhow::Error),
    /// Stopped before or during its conversion: neither sent nor refused.
    Stopped,
    /// The conversion panicked: the run stops, as a panic on its own thread
    /// would stop it.
    Panicked(String),
}

type Convert = dyn Fn(&Job, &dyn Fn() -> bool) -> Result<Converted> + Send + Sync;
type Room = dyn Fn() -> Result<u64> + Send + Sync;

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
    cancelled: Arc<AtomicBool>,
    room: Box<Room>,
    convert: Box<Convert>,
}

struct State {
    /// Not started, in member order.
    jobs: VecDeque<Job>,
    /// Twice the FLAC bytes of the conversions under way.
    reserved: u64,
    /// Members below this index belong to the current batch or an earlier
    /// one: their conversions start whatever the room.
    current: usize,
    /// Members from this index on wait for a later batch to begin.
    horizon: usize,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed)
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The conversions of one run's FLACs, by worker threads that take them in
/// member order.
pub(super) struct Lookahead {
    shared: Arc<Shared>,
    workers: Vec<thread::JoinHandle<()>>,
    results: mpsc::UnboundedReceiver<(usize, Outcome)>,
    /// Finished, not yet taken by a batch.
    finished: BTreeMap<usize, Outcome>,
    /// Batch size: the next batch ends this many members after the current.
    window: usize,
    /// The members that convert, in order.
    flacs: Vec<usize>,
    /// For a batch's room check message.
    directory: PathBuf,
}

/// Up to [`THREADS`], leaving one processor free, and at least one.
fn threads() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .saturating_sub(1)
        .clamp(1, THREADS)
}

impl Lookahead {
    /// Convert `members`' FLACs with FFmpeg into `converter`'s directory,
    /// for batches of `window` members. None when no member converts.
    pub(super) fn ffmpeg(
        members: &[Member],
        window: usize,
        converter: &flac_to_alac::Converter,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Option<Self>> {
        let ffmpeg = converter.ffmpeg.clone();
        let directory = converter.directory.path().to_owned();
        let room = directory.clone();
        let into = directory.clone();
        Self::start(
            members,
            window,
            threads(),
            cancelled,
            &directory,
            move || flac_to_alac::available(&room),
            move |job, abandoned| {
                flac_to_alac::convert_into(&ffmpeg, &job.flac, &job.prepared, &into, abandoned)
            },
        )
    }

    /// Convert `members`' FLACs (`convert`) on `threads` workers with
    /// `convert`, which puts each into `directory`; `room` is the free space
    /// there. Nothing starts before the first [`Self::batch`]; a cancel stops
    /// the conversions.
    pub(super) fn start(
        members: &[Member],
        window: usize,
        threads: usize,
        cancelled: Arc<AtomicBool>,
        directory: &Path,
        room: impl Fn() -> Result<u64> + Send + Sync + 'static,
        convert: impl Fn(&Job, &dyn Fn() -> bool) -> Result<Converted> + Send + Sync + 'static,
    ) -> Result<Option<Self>> {
        ensure!(window > 0, "batch size must be positive");
        let mut jobs = VecDeque::new();
        for (index, member) in members.iter().enumerate().filter(|(_, m)| m.convert) {
            let Audio::Mpeg(prepared) = &member.audio else {
                bail!("{} is not a FLAC", member.relative_path);
            };
            jobs.push_back(Job {
                index,
                flac: member.source.clone(),
                prepared: Arc::clone(prepared),
                cover: member.cover.as_ref().map(|c| c.sha256),
                bytes: prepared.identity().file_bytes,
            });
        }
        if jobs.is_empty() {
            return Ok(None);
        }
        let flacs = jobs.iter().map(|job| job.index).collect();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                jobs,
                reserved: 0,
                current: 0,
                horizon: 0,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            cancelled,
            room: Box::new(room),
            convert: Box::new(convert),
        });
        let (sender, results) = mpsc::unbounded_channel();
        let mut lookahead = Self {
            shared,
            workers: Vec::new(),
            results,
            finished: BTreeMap::new(),
            window,
            flacs,
            directory: directory.to_owned(),
        };
        // A worker that cannot start stops the others (on drop).
        for number in 0..threads.max(1) {
            let shared = Arc::clone(&lookahead.shared);
            let sender = sender.clone();
            lookahead.workers.push(
                thread::Builder::new()
                    .name(format!("flac-to-alac-{number}"))
                    .spawn(move || work(&shared, &sender))?,
            );
        }
        Ok(Some(lookahead))
    }

    /// Where a batch from `offset` ends, at most at `end`: after its
    /// [`FLACS`]th FLAC, and before the first whose conversion, with those of
    /// the FLACs before it, would not fit in the room left, twice their size.
    /// A batch holds at least one member, so a FLAC that cannot fit alone gets
    /// [`Self::batch`]'s refusal.
    pub(super) fn fitting(&self, offset: usize, end: usize) -> Result<usize> {
        let end = end.min(self.after_flacs(offset));
        let state = self.shared.lock();
        let available = (self.shared.room)()?.saturating_sub(state.reserved);
        let mut jobs = state
            .jobs
            .iter()
            .skip_while(|job| job.index < offset)
            .peekable();
        let mut needed = 0u64;
        for index in offset..end {
            if let Some(job) = jobs.next_if(|job| job.index == index) {
                needed = needed.saturating_add(job.bytes.saturating_mul(2));
                if needed > available {
                    return Ok(index.max(offset + 1));
                }
            }
        }
        Ok(end)
    }

    /// The member index past the [`FLACS`]th FLAC from `index` on.
    fn after_flacs(&self, index: usize) -> usize {
        let first = self.flacs.partition_point(|&flac| flac < index);
        self.flacs.get(first + FLACS).copied().unwrap_or(usize::MAX)
    }

    /// Begin the batch `chunk`, the run's members from `offset` on, and wait
    /// for all its FLACs. Each conversion then takes its member's place; a
    /// FLAC whose conversion fails, loses audio or carries another cover is
    /// refused (`refused`) and moved to the end of the chunk, the others
    /// keeping their order. Returns how many members are left to send. Not
    /// enough room for the FLACs not yet converting, or a cancel while
    /// waiting, stops the run before the batch takes any conversion.
    pub(super) async fn batch(
        &mut self,
        chunk: &mut [Member],
        offset: usize,
        refused: &mut impl FnMut(&Member, &anyhow::Error) -> Result<()>,
    ) -> Result<usize> {
        let end = offset + chunk.len();
        {
            let mut state = self.shared.lock();
            // Every earlier batch waited for its own: what is left starts here.
            let needed = state
                .jobs
                .iter()
                .take_while(|job| job.index < end)
                .map(|job| job.bytes)
                .sum::<u64>()
                .saturating_mul(2);
            if needed > 0 {
                let available = (self.shared.room)()?.saturating_sub(state.reserved);
                ensure!(
                    available >= needed,
                    "converting this batch's FLACs needs about {} MiB in {}, which has {} MiB free; set TMPDIR to a directory with more room. Nothing was sent in this batch",
                    needed.div_ceil(1 << 20),
                    self.directory.display(),
                    available >> 20
                );
            }
            state.current = end;
            state.horizon = end.saturating_add(self.window).min(self.after_flacs(end));
        }
        self.shared.changed.notify_all();
        // Every conversion of the batch, before any is taken: a cancel, or one
        // that stopped or panicked, leaves them all to be removed.
        let indexes: Vec<usize> = (offset..end)
            .filter(|index| chunk[index - offset].convert)
            .collect();
        while !indexes
            .iter()
            .all(|index| self.finished.contains_key(index))
        {
            cancelled(&self.shared)?;
            match tokio::time::timeout(POLL, self.results.recv()).await {
                Ok(Some((index, outcome))) => {
                    self.finished.insert(index, outcome);
                }
                Ok(None) => bail!("the FLAC conversions ended early"),
                Err(_) => {}
            }
        }
        cancelled(&self.shared)?;
        for index in &indexes {
            let member = &chunk[index - offset];
            match &self.finished[index] {
                Outcome::Stopped => bail!("the conversion of {} stopped", member.relative_path),
                Outcome::Panicked(message) => bail!(
                    "converting {} failed unexpectedly: {message}",
                    member.relative_path
                ),
                Outcome::Converted(_) | Outcome::Refused(_) => {}
            }
        }
        for index in indexes {
            let member = &mut chunk[index - offset];
            match self.finished.remove(&index) {
                Some(Outcome::Converted(converted)) => {
                    let converted = *converted;
                    member.cover = member.cover.take().and(converted.cover);
                    member.source = converted.path;
                    member.audio = Audio::Alac {
                        fingerprint: converted.fingerprint,
                        metadata: converted.metadata,
                    };
                    member.conversion = Some(converted.conversion);
                    member.convert = false;
                }
                Some(Outcome::Refused(error)) => refused(member, &error)?,
                _ => bail!("the conversion of {} is missing", member.relative_path),
            }
        }
        // Refused FLACs (still `convert`) go last; the sort is stable.
        chunk.sort_by_key(|m| m.convert);
        Ok(chunk.iter().filter(|m| !m.convert).count())
    }
}

/// A cancel stops the batch as it stops a session.
fn cancelled(shared: &Shared) -> Result<()> {
    ensure!(
        !shared.cancelled.load(Ordering::Relaxed),
        "insertion cancelled; reconcile any Started attempt"
    );
    Ok(())
}

impl Drop for Lookahead {
    /// Kill the conversions under way, wait for their threads, and remove
    /// every conversion no batch took.
    fn drop(&mut self) {
        // Under the lock, so that no worker misses it before it waits.
        {
            let _state = self.shared.lock();
            self.shared.stop.store(true, Ordering::Relaxed);
        }
        self.shared.changed.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        while let Ok((index, outcome)) = self.results.try_recv() {
            self.finished.insert(index, outcome);
        }
        for outcome in std::mem::take(&mut self.finished).into_values() {
            if let Outcome::Converted(converted) = outcome {
                let _ = flac_to_alac::remove(&converted.path);
            }
        }
    }
}

/// One worker: take the next job once its batch allows it, convert it, and
/// hand the outcome back, until no job is left or the run stops.
fn work(shared: &Shared, results: &mpsc::UnboundedSender<(usize, Outcome)>) {
    let mut state = shared.lock();
    loop {
        if shared.stopped() {
            return;
        }
        let Some(job) = state.jobs.front() else {
            return;
        };
        let need = job.bytes.saturating_mul(2);
        let ready = job.index < state.current
            || (job.index < state.horizon
                && (shared.room)().is_ok_and(|room| room.saturating_sub(state.reserved) >= need));
        if !ready {
            state = shared
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            continue;
        }
        let Some(job) = state.jobs.pop_front() else {
            return;
        };
        state.reserved += need;
        drop(state);
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| convert(shared, &job)))
                .unwrap_or_else(|panic| {
                    Outcome::Panicked(
                        panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_default(),
                    )
                });
        state = shared.lock();
        state.reserved -= need;
        if let Err(mpsc::error::SendError((_, Outcome::Converted(converted)))) =
            results.send((job.index, outcome))
        {
            let _ = flac_to_alac::remove(&converted.path);
        }
        shared.changed.notify_all();
    }
}

fn convert(shared: &Shared, job: &Job) -> Outcome {
    let abandoned = || shared.stopped();
    if abandoned() {
        return Outcome::Stopped;
    }
    let converted = (shared.convert)(job, &abandoned).and_then(|converted| {
        // The FLAC's cover as sent (a PNG's JPEG), now with the M4A's
        // checksum.
        if let Some(cover) = job.cover
            && converted.cover.as_ref().map(|c| c.sha256) != Some(cover)
        {
            flac_to_alac::remove(&converted.path)?;
            bail!("the ALAC's cover differs from the FLAC's");
        }
        Ok(converted)
    });
    match converted {
        Ok(converted) => Outcome::Converted(Box::new(converted)),
        Err(_) if abandoned() => Outcome::Stopped,
        Err(error) => Outcome::Refused(error),
    }
}

#[cfg(test)]
mod tests;
