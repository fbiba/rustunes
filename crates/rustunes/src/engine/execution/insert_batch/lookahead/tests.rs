//! The lookahead with a converter that only pretends: each conversion takes
//! the time, fails, holds or carries the cover a test asks of it, and writes
//! a stand-in file into a real conversion directory.
use super::*;
use music_protocol::media::artwork::Cover;
use sha2::Digest;
use std::{
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

const FLAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/tone-flac.flac"
);
const ALAC: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/tone-alac.m4a"
);

/// What the fake does with one member's FLAC.
#[derive(Clone, Copy)]
enum Act {
    /// Converts after this many milliseconds.
    After(u64),
    /// Fails after this many milliseconds.
    Fail(u64),
    /// Converts once released, or stops once abandoned.
    Hold,
    /// Converts, with another cover than the FLAC's.
    OtherCover,
    /// Panics.
    Panic,
}

struct Fake {
    acts: BTreeMap<usize, Act>,
    directory: PathBuf,
    started: Mutex<Vec<usize>>,
    /// Holds abandoned while converting.
    stopped: Mutex<Vec<usize>>,
    released: AtomicBool,
}

impl Fake {
    fn new(directory: &Path, acts: &[(usize, Act)]) -> Arc<Self> {
        Arc::new(Self {
            acts: acts.iter().copied().collect(),
            directory: directory.to_owned(),
            started: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
            released: AtomicBool::new(false),
        })
    }

    fn started(&self) -> Vec<usize> {
        let mut started = self.started.lock().unwrap().clone();
        started.sort();
        started
    }

    fn output(&self, index: usize) -> PathBuf {
        self.directory.join(format!("{index}.m4a"))
    }

    fn convert(&self, job: &Job, abandoned: &dyn Fn() -> bool) -> Result<Converted> {
        self.started.lock().unwrap().push(job.index);
        let act = self.acts.get(&job.index).copied().unwrap_or(Act::After(0));
        let pause = |ms: u64| -> Result<()> {
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                ensure!(!abandoned(), "stopped");
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        };
        match act {
            Act::After(ms) => pause(ms)?,
            Act::Fail(ms) => {
                pause(ms)?;
                bail!("member {} cannot be converted", job.index);
            }
            Act::Hold => {
                while !self.released.load(Ordering::Relaxed) {
                    if abandoned() {
                        self.stopped.lock().unwrap().push(job.index);
                        bail!("stopped");
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Act::OtherCover => {}
            Act::Panic => panic!("member {} broke the converter", job.index),
        }
        let path = self.output(job.index);
        std::fs::write(&path, b"alac")?;
        let cover = match act {
            Act::OtherCover => Some(cover(b"other")),
            _ => job.cover.map(|sha256| Cover {
                bytes: Vec::new(),
                sha256,
                checksum: 1,
            }),
        };
        Ok(Converted {
            path,
            fingerprint: music_protocol::media::fingerprint::read(Path::new(ALAC))?,
            metadata: music_protocol::media::metadata::read(Path::new(ALAC))?,
            cover,
            conversion: crate::engine::state::journal::Conversion {
                source_path: job.flac.clone(),
                source_bytes: job.bytes,
                source_sha256: [0; 32],
                source_audio: [0; 32],
                ffmpeg: "fake".into(),
            },
        })
    }
}

fn cover(bytes: &[u8]) -> Cover {
    Cover {
        sha256: sha2::Sha256::digest(bytes).into(),
        checksum: bytes.len() as u32,
        bytes: bytes.to_vec(),
    }
}

/// A FLAC member still to convert, under `name`.
fn flac(name: &str, cover: Option<Cover>) -> Member {
    let path = Path::new(FLAC);
    let source = music_protocol::media::mpeg::read(path).unwrap();
    Member {
        relative_path: name.into(),
        source: path.to_owned(),
        audio: Audio::Mpeg(Arc::new(
            PreparedSource::read_expected(path, &source).unwrap(),
        )),
        track: 1,
        album: 2,
        artist: 3,
        destination: format!("iTunes_Control/Music/F00/{name}.m4a"),
        cover,
        keep_album_image: false,
        conversion: None,
        convert: true,
    }
}

/// An ALAC member, nothing to convert.
fn alac(name: &str) -> Member {
    let path = Path::new(ALAC);
    Member {
        audio: Audio::Alac {
            fingerprint: music_protocol::media::fingerprint::read(path).unwrap(),
            metadata: music_protocol::media::metadata::read(path).unwrap(),
        },
        convert: false,
        source: path.to_owned(),
        ..flac(name, None)
    }
}

fn flac_bytes() -> u64 {
    std::fs::metadata(FLAC).unwrap().len()
}

fn names(members: &[Member]) -> Vec<&str> {
    members.iter().map(|m| m.relative_path.as_str()).collect()
}

/// Wait up to five seconds for `condition`.
fn eventually(condition: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

struct Run {
    fake: Arc<Fake>,
    room: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    directory: flac_to_alac::Directory,
}

impl Run {
    fn new(acts: &[(usize, Act)]) -> Self {
        let directory = flac_to_alac::Directory::create().unwrap();
        Self {
            fake: Fake::new(directory.path(), acts),
            room: Arc::new(AtomicU64::new(u64::MAX)),
            cancelled: Arc::new(AtomicBool::new(false)),
            directory,
        }
    }

    fn start(&self, members: &[Member], window: usize, threads: usize) -> Lookahead {
        let room = Arc::clone(&self.room);
        let fake = Arc::clone(&self.fake);
        Lookahead::start(
            members,
            window,
            threads,
            Arc::clone(&self.cancelled),
            self.directory.path(),
            move || Ok(room.load(Ordering::Relaxed)),
            move |job, abandoned| fake.convert(job, abandoned),
        )
        .unwrap()
        .unwrap()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.directory.path());
    }
}

type Refusals = Vec<(String, String)>;

async fn batch(
    lookahead: &mut Lookahead,
    chunk: &mut [Member],
    offset: usize,
) -> Result<(usize, Refusals)> {
    let mut refusals = Vec::new();
    let sendable = lookahead
        .batch(chunk, offset, &mut |member, error| {
            refusals.push((member.relative_path.clone(), format!("{error:#}")));
            Ok(())
        })
        .await?;
    Ok((sendable, refusals))
}

#[test]
fn nothing_to_convert_needs_no_lookahead() {
    let run = Run::new(&[]);
    let members = [alac("a"), alac("b")];
    let room = || Ok(0);
    let convert = |_: &Job, _: &dyn Fn() -> bool| -> Result<Converted> { bail!("never") };
    assert!(
        Lookahead::start(
            &members,
            1,
            3,
            Arc::clone(&run.cancelled),
            run.directory.path(),
            room,
            convert
        )
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn conversions_take_their_members_places_whatever_order_they_finish_in() {
    let front = cover(b"front");
    let run = Run::new(&[
        (0, Act::After(300)),
        (2, Act::Fail(250)),
        (3, Act::OtherCover),
        (5, Act::Fail(0)),
    ]);
    let mut chunk = vec![
        flac("slow", Some(front.clone())),
        alac("plain"),
        flac("failing", None),
        flac("other cover", Some(front.clone())),
        flac("fast", None),
        flac("failing fast", None),
    ];
    let mut lookahead = run.start(&chunk, 6, 3);
    let (sendable, refusals) = batch(&mut lookahead, &mut chunk, 0).await.unwrap();
    assert_eq!(sendable, 3);
    // The refused go last; both sets keep their order, and the refusals come
    // in member order, not in the order the conversions ended.
    assert_eq!(
        names(&chunk),
        [
            "slow",
            "plain",
            "fast",
            "failing",
            "other cover",
            "failing fast"
        ]
    );
    let refused: Vec<_> = refusals.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(refused, ["failing", "other cover", "failing fast"]);
    assert!(refusals[0].1.contains("member 2 cannot be converted"));
    assert!(refusals[1].1.contains("cover differs"));
    // A cover that differs is not kept.
    assert!(!run.fake.output(3).exists());
    // Each conversion is its own member's.
    for (member, index) in chunk.iter().zip([0, usize::MAX, 4]) {
        if index == usize::MAX {
            assert!(member.conversion.is_none() && !member.convert);
            continue;
        }
        assert!(!member.convert);
        assert_eq!(member.source, run.fake.output(index));
        assert!(matches!(member.audio, Audio::Alac { .. }));
        assert_eq!(member.conversion.as_ref().unwrap().ffmpeg, "fake");
    }
    assert_eq!(chunk[0].cover.as_ref().unwrap().sha256, front.sha256);
    // The refused stay FLACs to convert, with their covers.
    assert!(
        chunk[3..]
            .iter()
            .all(|m| m.convert && m.conversion.is_none())
    );
    assert!(chunk[4].cover.is_some());
    drop(lookahead);
    // The batch took its conversions: the end of the run leaves them.
    assert!(run.fake.output(0).exists() && run.fake.output(4).exists());
}

#[tokio::test]
async fn the_next_batch_converts_while_the_current_one_is_sent() {
    let run = Run::new(&[(3, Act::Fail(0))]);
    let mut members: Vec<_> = (0..6).map(|n| flac(&format!("{n}"), None)).collect();
    let mut lookahead = run.start(&members, 2, 3);
    // Nothing starts before the first batch checks its room.
    thread::sleep(POLL * 2);
    assert!(run.fake.started().is_empty());
    let (chunk, rest) = members.split_at_mut(2);
    assert_eq!(batch(&mut lookahead, chunk, 0).await.unwrap(), (2, vec![]));
    // While the first batch is sent, the second converts, and no further.
    eventually(|| run.fake.started() == [0, 1, 2, 3]);
    eventually(|| lookahead.shared.lock().reserved == 0);
    thread::sleep(POLL * 2);
    assert_eq!(run.fake.started(), [0, 1, 2, 3]);
    // Its failure is reported only when its batch begins.
    let (chunk, rest) = rest.split_at_mut(2);
    let (sendable, refusals) = batch(&mut lookahead, chunk, 2).await.unwrap();
    assert_eq!(sendable, 1);
    assert!(matches!(&refusals[..], [(name, _)] if name == "3"));
    eventually(|| run.fake.started() == [0, 1, 2, 3, 4, 5]);
    assert_eq!(batch(&mut lookahead, rest, 4).await.unwrap(), (2, vec![]));
}

#[tokio::test]
async fn a_batch_without_room_for_its_flacs_sends_nothing() {
    let run = Run::new(&[]);
    let mut members = vec![flac("a", None), flac("b", None)];
    run.room.store(4 * flac_bytes() - 1, Ordering::Relaxed);
    let mut lookahead = run.start(&members, 2, 3);
    let error = batch(&mut lookahead, &mut members, 0).await.unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("Nothing was sent in this batch"), "{error}");
    assert!(error.contains(&run.directory.path().display().to_string()));
    thread::sleep(POLL * 2);
    assert!(run.fake.started().is_empty());
    // Exactly enough room: the batch goes on.
    run.room.store(4 * flac_bytes(), Ordering::Relaxed);
    assert_eq!(
        batch(&mut lookahead, &mut members, 0).await.unwrap(),
        (2, vec![])
    );
}

#[tokio::test]
async fn a_batch_ends_before_the_flac_whose_conversion_would_not_fit() {
    let run = Run::new(&[]);
    let mut members = vec![flac("a", None), alac("b"), flac("c", None), flac("d", None)];
    // Room for two conversions: a and c, with b between them.
    run.room.store(4 * flac_bytes() + 1, Ordering::Relaxed);
    let mut lookahead = run.start(&members, 4, 3);
    assert_eq!(lookahead.fitting(0, 4).unwrap(), 3);
    assert_eq!(lookahead.fitting(0, 2).unwrap(), 2);
    // A FLAC that cannot fit alone is a batch of its own, which refuses.
    run.room.store(0, Ordering::Relaxed);
    assert_eq!(lookahead.fitting(0, 4).unwrap(), 1);
    run.room.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(lookahead.fitting(0, 4).unwrap(), 4);
    assert_eq!(
        batch(&mut lookahead, &mut members, 0).await.unwrap(),
        (4, vec![])
    );
}

#[test]
fn a_batch_holds_at_most_100_flacs_and_converts_as_many_ahead() {
    let run = Run::new(&[]);
    let mut members: Vec<_> = (0..202).map(|n| flac(&n.to_string(), None)).collect();
    members.insert(50, alac("between"));
    let lookahead = run.start(&members, 499, 3);
    // The 100 FLACs, with the ALAC among them.
    assert_eq!(lookahead.fitting(0, 499).unwrap(), 101);
    assert_eq!(lookahead.fitting(0, 60).unwrap(), 60);
    assert_eq!(lookahead.after_flacs(101), 201);
    assert_eq!(lookahead.after_flacs(201), usize::MAX);
}

#[tokio::test]
async fn a_conversion_ahead_waits_for_room_beside_the_others() {
    let run = Run::new(&[(0, Act::Hold), (1, Act::Hold), (2, Act::Hold)]);
    let mut members: Vec<_> = (0..4).map(|n| flac(&format!("{n}"), None)).collect();
    // Room for the first batch's three, each reserving twice its FLAC.
    run.room.store(6 * flac_bytes(), Ordering::Relaxed);
    let mut lookahead = run.start(&members, 3, 4);
    let (chunk, rest) = members.split_at_mut(3);
    let first = {
        let fake = Arc::clone(&run.fake);
        let room = Arc::clone(&run.room);
        thread::spawn(move || {
            eventually(|| fake.started() == [0, 1, 2]);
            // The fourth thread is idle, but the three under way hold the
            // room the one ahead needs.
            thread::sleep(POLL * 2);
            assert_eq!(fake.started(), [0, 1, 2]);
            // Their files then fill the directory (the room is measured, not
            // assumed).
            room.store(2 * flac_bytes() - 1, Ordering::Relaxed);
            fake.released.store(true, Ordering::Relaxed);
        })
    };
    assert_eq!(batch(&mut lookahead, chunk, 0).await.unwrap(), (3, vec![]));
    first.join().unwrap();
    // The one ahead still waits, and its batch cannot begin.
    thread::sleep(POLL * 2);
    assert_eq!(run.fake.started(), [0, 1, 2]);
    let error = batch(&mut lookahead, rest, 3).await.unwrap_err();
    assert!(format!("{error:#}").contains("Nothing was sent"));
    // A settled batch's conversions are removed: it starts.
    run.room.store(2 * flac_bytes(), Ordering::Relaxed);
    eventually(|| run.fake.started() == [0, 1, 2, 3]);
    assert_eq!(batch(&mut lookahead, rest, 3).await.unwrap(), (1, vec![]));
}

#[tokio::test]
async fn a_cancel_stops_the_conversions_and_removes_those_ahead() {
    let run = Run::new(&[(3, Act::Hold)]);
    let mut members: Vec<_> = (0..4).map(|n| flac(&format!("{n}"), None)).collect();
    let mut lookahead = run.start(&members, 2, 3);
    let (chunk, rest) = members.split_at_mut(2);
    batch(&mut lookahead, chunk, 0).await.unwrap();
    // One conversion ahead is done, one under way.
    eventually(|| run.fake.output(2).exists() && run.fake.started().contains(&3));
    run.cancelled.store(true, Ordering::Relaxed);
    let error = batch(&mut lookahead, rest, 2).await.unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    // The one under way saw the cancel.
    eventually(|| *run.fake.stopped.lock().unwrap() == [3]);
    drop(lookahead);
    // What no batch took is gone; the first batch's stay for it.
    assert!(!run.fake.output(2).exists() && !run.fake.output(3).exists());
    assert!(run.fake.output(0).exists() && run.fake.output(1).exists());
    assert!(rest.iter().all(|m| m.convert));
}

#[tokio::test]
async fn an_error_mid_run_kills_the_conversion_under_way_and_removes_those_ahead() {
    let run = Run::new(&[(3, Act::Hold)]);
    let mut members: Vec<_> = (0..4).map(|n| flac(&format!("{n}"), None)).collect();
    let mut lookahead = run.start(&members, 2, 3);
    batch(&mut lookahead, &mut members[..2], 0).await.unwrap();
    eventually(|| run.fake.output(2).exists() && run.fake.started().contains(&3));
    // The batch fails (here: is dropped) while the next converts.
    let started = Instant::now();
    drop(lookahead);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(*run.fake.stopped.lock().unwrap(), [3]);
    assert!(!run.fake.output(2).exists() && !run.fake.output(3).exists());
    assert!(run.fake.output(0).exists() && run.fake.output(1).exists());
}

#[tokio::test]
async fn a_conversion_that_panics_stops_the_run_instead_of_hanging_it() {
    let run = Run::new(&[(1, Act::Panic)]);
    let mut members: Vec<_> = (0..3).map(|n| flac(&format!("{n}"), None)).collect();
    let mut lookahead = run.start(&members, 3, 3);
    let error = batch(&mut lookahead, &mut members, 0).await.unwrap_err();
    let error = format!("{error:#}");
    assert!(
        error.contains("converting 1 failed unexpectedly: member 1 broke the converter"),
        "{error}"
    );
    // Nothing was taken: the end of the run removes what the others made.
    drop(lookahead);
    assert!(!run.fake.output(0).exists() && !run.fake.output(2).exists());
    assert!(members.iter().all(|m| m.convert));
}
