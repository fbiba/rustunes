//! Pull the journal from the phone. A pull captures the phone fresh and
//! writes a new journal binding every phone track it can account for: one an
//! earlier journal bound, confirmed by the capture (carried, with its cover
//! record and source path), or one whose audio a library file holds
//! (matched, and managed from then on, as if this tool had sent it). With
//! `tracks_into`, a phone track no library file holds is first copied into
//! that directory and bound to its copy.
//!
//! The earlier journal is read by `read_prior`, so it may come from a build
//! with another schema or other hash definitions. It stays beside the new one
//! as `journal.previous`. A pull sends nothing to the phone. `sync` and `push`
//! pull first, without being asked, when the journal is superseded or when
//! only an earlier journal at another location (other identity digests) knows
//! the phone's tracks: a new version keeps the library it finds.
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    engine::execution::device,
    engine::execution::native_insert::Control,
    engine::planning::source_collection::{self, SelectionOptions, SourceCollection},
    engine::state::audio_index::OtherCodec,
    engine::state::current,
    engine::state::journal::{Artwork, Content, Identity, Journal, Prior, Pulled},
    engine::state::layout,
    engine::state::snapshot,
    engine::verification::artwork::{self, TrackArtworkExpectation, TrackCheck},
    engine::verification::audio_capture,
};
use music_protocol::media::fingerprint::TransferredSource;
use music_protocol::protocol::afc::copy;
use music_protocol::protocol::trace::private_write;

pub struct Request<'a> {
    pub state: &'a Path,
    /// The library directory whose files phone tracks are matched to.
    pub library: Option<&'a Path>,
    /// Match to the files in its subfolders too.
    pub recursive: bool,
    /// Report the pull and keep the journal it would write in the run
    /// directory; the journal in use is left as it is.
    pub dry_run: bool,
    pub seconds: u64,
    /// `pull --tracks`: copy each phone track no library file holds into
    /// this directory first, then bind it there.
    pub tracks_into: Option<&'a Path>,
}

/// A phone track the pull left unbound, or copied.
#[derive(Clone, Debug, Serialize)]
pub struct PhoneTrack {
    pub track_id: String,
    pub artist: String,
    pub title: String,
    /// Where `pull --tracks` copied it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copied_to: Option<PathBuf>,
    /// Why it is not bound, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub run: PathBuf,
    /// The journal the pull wrote (in the run directory for a dry run).
    pub journal: PathBuf,
    /// The earlier journal it carried forward, and whether another version
    /// of rustunes wrote it.
    pub earlier: Option<PathBuf>,
    pub superseded: bool,
    pub phone_tracks: usize,
    /// Bindings of the earlier journal the capture confirmed.
    pub carried: usize,
    /// Phone tracks newly bound to a library file.
    pub matched: usize,
    /// Phone tracks `--tracks` copied into the library and bound.
    pub copied: Vec<PhoneTrack>,
    /// Phone tracks left unbound: no library file holds their audio.
    pub unbound: Vec<PhoneTrack>,
    /// Bindings of the earlier journal whose track the phone no longer holds.
    pub gone: usize,
    /// Phone covers read back as the image their binding expects, under a
    /// cache ID the earlier journal did not record (the phone was cleared
    /// and the cover sent again) or for a newly matched track.
    pub covers_proven: usize,
    /// Phone covers that could not be proven so; the next sync sends them.
    pub covers_unproven: usize,
    pub unfinished: Vec<String>,
    pub unreadable: Vec<String>,
    /// Whether the new journal is now the one in use.
    pub installed: bool,
}

pub(crate) async fn run(request: Request<'_>, engine: &crate::engine::Engine) -> Result<Report> {
    engine.check()?;
    ensure!(
        (1..=1800).contains(&request.seconds),
        "capture deadline must be 1..1800 seconds"
    );
    if let Some(into) = request.tracks_into {
        ensure!(
            fs::metadata(into).is_ok_and(|m| m.is_dir()),
            "{} is not a directory to copy tracks into",
            into.display()
        );
    }
    let options = SelectionOptions {
        recursive: request.recursive,
        ..Default::default()
    };
    let mut library = request
        .library
        .map(|path| engine.with_observer(|| inspect_library(path, &options)))
        .transpose()?;
    // Files an earlier `pull --tracks` placed outside the library are matched
    // too, so a second pull does not copy them again.
    if let Some(into) = request.tracks_into {
        let into = fs::canonicalize(into)?;
        match &mut library {
            Some(library) if library.holds(&into) => {}
            Some(library) => {
                let placed = engine.with_observer(|| inspect_library(&into, &options))?;
                library.files.extend(placed.files);
            }
            None => library = Some(engine.with_observer(|| inspect_library(&into, &options))?),
        }
    }
    let run = layout::new_run(request.state)?;
    engine.emit(crate::engine::Event::RunEvidence(&run))?;
    engine.emit(crate::engine::Event::ReadingDeviceSnapshot)?;
    let path = run.join("snapshot");
    let absent = crate::engine::state::audio_index::Absent {
        gone: true,
        ..crate::engine::state::audio_index::Absent::NONE
    };
    audio_capture::capture_allowing(
        &path,
        absent,
        &engine.control(request.seconds),
        device::provider,
    )
    .await?;
    let saved = snapshot::open_allowing(&path, absent)?;
    let identity = identity(&saved)?;
    let journal = layout::journal_path(
        request.state,
        &saved.identity.device_identifier_sha256,
        &saved.identity.library_identity_sha256,
    )?;
    let (earlier, superseded, prior) = match find_prior(
        request.state,
        &journal,
        &identity,
        matcher(&saved),
        &run,
    )? {
        Some(found) => {
            if !found.superseded && !found.prior.unfinished.is_empty() {
                bail!(
                    "the journal has {}; settle it first with `rustunes resume` or by running the command that started it again, then pull",
                    found.prior.unfinished.join(" and ")
                );
            }
            (Some(found.path), found.superseded, Some(found.prior))
        }
        None => (None, false, None),
    };
    let mut reconciled = reconcile_saved(&saved, prior.as_ref(), library.as_ref());
    // A dry run copies nothing: the tracks it would copy are listed unbound.
    if let Some(into) = request.tracks_into.filter(|_| !request.dry_run) {
        copy_unbound(
            &mut reconciled,
            &saved,
            into,
            library.as_ref(),
            engine,
            request.seconds,
        )
        .await?;
    }
    let covers = prove_covers(
        &mut reconciled,
        &identity,
        &run,
        &engine.control(request.seconds),
    )
    .await?;
    let target = if request.dry_run {
        run.join("journal")
    } else {
        journal.clone()
    };
    write(
        &target,
        &identity,
        receipt(&saved),
        &reconciled.pulled,
        prior.as_ref(),
        request.dry_run,
    )?;
    let report = Report {
        schema: "rustunes.pull",
        run,
        journal: target,
        earlier,
        superseded,
        phone_tracks: reconciled.phone_tracks,
        carried: reconciled.carried,
        matched: reconciled.matched,
        copied: reconciled.copied,
        unbound: reconciled.unbound,
        gone: reconciled.gone,
        covers_proven: covers.proven,
        covers_unproven: covers.unproven,
        unfinished: prior
            .as_ref()
            .map(|p| p.unfinished.clone())
            .unwrap_or_default(),
        unreadable: prior
            .as_ref()
            .map(|p| p.unreadable.clone())
            .unwrap_or_default(),
        installed: !request.dry_run,
    };
    private_write(
        &report.run.join("pull.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

/// The journal for this capture's phone and library, carried forward first
/// when this build cannot use the one there (`Superseded`) or when there is
/// none but an earlier journal elsewhere in `state` knows the phone's tracks.
/// A current journal, or no journal and nothing to carry, is left alone.
/// `library` is the directory phone tracks are matched to, with its
/// subfolders when `recursive`. Progress goes to the engine scope's observer.
pub(crate) async fn current_journal(
    state: &Path,
    saved: &snapshot::Snapshot,
    library: Option<&Path>,
    recursive: bool,
    run: &Path,
    control: &Control,
) -> Result<PathBuf> {
    let identity = identity(saved)?;
    let journal = layout::journal_path(
        state,
        &saved.identity.device_identifier_sha256,
        &saved.identity.library_identity_sha256,
    )?;
    if journal.try_exists()? {
        match Journal::open_read_only(&journal, &identity) {
            Ok(_) => return Ok(journal),
            Err(error) if crate::engine::state::journal::is_superseded(&error) => {}
            Err(error) => return Err(error),
        }
    }
    let Some(found) = find_prior(state, &journal, &identity, matcher(saved), run)? else {
        return Ok(journal);
    };
    let options = SelectionOptions {
        recursive,
        ..Default::default()
    };
    let library = library
        .map(|path| inspect_library(path, &options))
        .transpose()?;
    let mut reconciled = reconcile_saved(saved, Some(&found.prior), library.as_ref());
    let covers = prove_covers(&mut reconciled, &identity, run, control).await?;
    write(
        &journal,
        &identity,
        receipt(saved),
        &reconciled.pulled,
        Some(&found.prior),
        false,
    )?;
    crate::engine::observe(crate::engine::Event::JournalCarried {
        earlier: &found.path,
        journal: &journal,
        carried: reconciled.carried,
        matched: reconciled.matched,
        unbound: reconciled.unbound.len(),
        gone: reconciled.gone,
        covers_proven: covers.proven,
        covers_unproven: covers.unproven,
        unfinished: &found.prior.unfinished,
    })?;
    Ok(journal)
}

/// The capture a pull's bindings were verified against.
fn receipt(saved: &snapshot::Snapshot) -> (&[u8; 32], u32) {
    (
        &saved.index_receipt_sha256,
        saved.catalog.persisted_media_anchor(),
    )
}

fn identity(saved: &snapshot::Snapshot) -> Result<Identity> {
    Identity::for_saved_device(
        saved.identity.device_identifier_sha256,
        &music_protocol::catalogue::identity::parse(&saved.raw_preferences)?,
    )
}

/// The library's files, fingerprinted (from the source cache where unchanged).
fn inspect_library(path: &Path, options: &SelectionOptions) -> Result<Library> {
    let selection = source_collection::select(path, options)?;
    ensure!(
        selection.root() == selection.base_directory(),
        "the library must be a directory, not a file"
    );
    let collection = source_collection::inspect_with_progress(&selection, |done, total| {
        crate::engine::observe(crate::engine::Event::InspectingSources { done, total })
    })?;
    let scope = crate::engine::planning::removal::collection_scope_sha256(&collection)?;
    Ok(Library::of(&collection, scope))
}

/// The library's files and what the phone holds when it holds one, and its
/// collection scope, which bindings to its files record.
struct Library {
    base: PathBuf,
    files: Vec<(PathBuf, TransferredSource)>,
    scope: [u8; 32],
}
impl Library {
    fn of(collection: &SourceCollection, scope: [u8; 32]) -> Self {
        let base = &collection.base_directory;
        let alac = collection
            .tracks
            .iter()
            .map(|t| (base.join(&t.relative_path), t.fingerprint.clone().into()));
        let other = collection.mp3_tracks.iter().filter_map(|t| {
            Some((
                base.join(&t.relative_path),
                t.source.transferred_identity().ok()?,
            ))
        });
        Self {
            base: base.clone(),
            files: alac.chain(other).collect(),
            scope,
        }
    }
    fn holds(&self, path: &Path) -> bool {
        path.starts_with(&self.base)
    }
}

struct Found {
    path: PathBuf,
    superseded: bool,
    prior: Prior,
}

/// The earlier journal this pull carries forward. The journal at `journal`,
/// when there is one; otherwise the one in `state` whose bindings the capture
/// confirms most often (another location means other identity digests), or
/// `journal.previous` left by an interrupted pull.
fn find_prior(
    state: &Path,
    journal: &Path,
    identity: &Identity,
    matches: impl Fn(u64, &str, &TransferredSource) -> bool,
    run: &Path,
) -> Result<Option<Found>> {
    let scratch = run.join("earlier-journals");
    fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    if journal.try_exists()? {
        let superseded = match Journal::open_read_only(journal, identity) {
            Ok(_) => false,
            Err(error) if crate::engine::state::journal::is_superseded(&error) => true,
            Err(error) => return Err(error),
        };
        return Ok(Some(Found {
            path: journal.to_path_buf(),
            superseded,
            prior: crate::engine::state::journal::read_prior(journal, &scratch.join("0"))?,
        }));
    }
    let mut candidates = vec![sibling(journal, "journal.previous")];
    if let Ok(devices) = fs::read_dir(state.join("devices")) {
        for device in devices.flatten() {
            if let Ok(libraries) = fs::read_dir(device.path()) {
                for library in libraries.flatten() {
                    candidates.push(library.path().join("journal"));
                }
            }
        }
    }
    let mut best: Option<(usize, Found)> = None;
    for (n, candidate) in candidates.into_iter().enumerate() {
        if candidate == journal || !candidate.join("journal.sqlite").is_file() {
            continue;
        }
        // An unreadable candidate is not this phone's journal.
        let Ok(prior) =
            crate::engine::state::journal::read_prior(&candidate, &scratch.join(n.to_string()))
        else {
            continue;
        };
        let confirmed = prior
            .bindings
            .iter()
            .filter(|b| matches(b.content.track_pid, &b.content.path, &b.content.source))
            .count();
        if confirmed > 0 && best.as_ref().is_none_or(|(most, _)| confirmed > *most) {
            // Any journal elsewhere this build cannot open as this phone's.
            let superseded = Journal::open_read_only(&candidate, identity).is_err();
            best = Some((
                confirmed,
                Found {
                    path: candidate,
                    superseded,
                    prior,
                },
            ));
        }
    }
    Ok(best.map(|(_, found)| found))
}

fn sibling(journal: &Path, name: &str) -> PathBuf {
    journal.with_file_name(name)
}

#[derive(Default)]
struct Reconciled {
    pulled: Vec<Pulled>,
    phone_tracks: usize,
    carried: usize,
    matched: usize,
    gone: usize,
    copied: Vec<PhoneTrack>,
    unbound: Vec<PhoneTrack>,
    covers: Vec<CoverCandidate>,
}

/// A pulled track's phone cover no binding proves yet: the cache ID the
/// capture registers for it, and the image its earlier binding recorded
/// under another cache ID, if any.
#[derive(Debug, PartialEq, Eq)]
struct CoverCandidate {
    pulled: usize,
    cache_id: u32,
    earlier: Option<(u64, [u8; 32])>,
}

#[derive(Default)]
struct Covers {
    proven: usize,
    unproven: usize,
}

/// Failed readbacks in a row, none proven between them, after which the
/// phone is taken to be unreadable now and the rest are left unproven.
const COVER_FAILURE_RUN: usize = 25;

/// The length and SHA-256 of the cover sync sends for the file at `path`,
/// read as planning reads it: an ALAC M4A's own cover, or for any other
/// source (MP3, FLAC, WAV, AIFF, an AAC M4A) the one an insert batch sends.
fn source_cover(path: &Path) -> Option<(u64, [u8; 32])> {
    use crate::engine::planning::source_collection::mp3;
    let other = music_protocol::media::mpeg::is_other_audio(path)
        || (crate::engine::planning::source_reads::fingerprint(path).is_err()
            && music_protocol::media::mpeg::read(path).is_ok());
    let cover = if other {
        mp3::artwork::batch_cover(path, &music_protocol::media::mpeg::read(path).ok()?).ok()?
    } else {
        crate::engine::planning::source_reads::artwork(path)
            .ok()?
            .cover
    }?;
    Some((cover.bytes.len() as u64, cover.sha256))
}

/// Read back each cover `reconciled` names, in groups sharing a connection,
/// against its source file's cover (what sync compares a binding's with),
/// or the image its earlier binding recorded when the source has none to
/// read. A cover read back exactly goes in its binding, so the next sync
/// keeps it instead of sending it again; one that differs or cannot be read
/// is left out, and that sync sends it. Nothing is written to the phone.
async fn prove_covers(
    reconciled: &mut Reconciled,
    identity: &Identity,
    run: &Path,
    control: &Control,
) -> Result<Covers> {
    let traces = run.join("covers");
    let mut checks = Vec::new();
    let mut owners = Vec::new();
    for candidate in std::mem::take(&mut reconciled.covers) {
        let pulled = &reconciled.pulled[candidate.pulled];
        let source = pulled.source.as_deref().and_then(source_cover);
        let expected = source.or(candidate.earlier);
        let Some((bytes, sha256)) = expected else {
            continue;
        };
        checks.push(TrackCheck {
            target: pulled.content.track_pid,
            expected: TrackArtworkExpectation {
                cache_id: candidate.cache_id,
                bytes,
                sha256,
            },
            trace: traces.join(format!("cover-{:04}", owners.len())),
        });
        owners.push(candidate.pulled);
    }
    let total = checks.len();
    let mut covers = Covers {
        proven: 0,
        unproven: total,
    };
    if checks.is_empty() {
        return Ok(covers);
    }
    fs::DirBuilder::new().mode(0o700).create(&traces)?;
    let current = current::capture_controlled(
        &traces.join("state"),
        identity,
        control.deadline,
        Arc::clone(&control.cancelled),
    )
    .await?;
    let (mut tried, mut failures, mut group) = (0, 0, 0);
    while !checks.is_empty() && failures < COVER_FAILURE_RUN {
        if control.cancelled.load(Ordering::Relaxed) {
            bail!("pull cancelled while reading covers back; no journal was written");
        }
        if Instant::now() >= control.deadline {
            break;
        }
        let group_control = Control {
            deadline: control
                .deadline
                .min(Instant::now() + Duration::from_secs(120)),
            cancelled: Arc::clone(&control.cancelled),
        };
        let result = artwork::verify_track_group(
            &current,
            &checks,
            &traces.join(format!("group-{group:04}")),
            &group_control,
            &BTreeSet::new(),
            device::provider,
            |position| {
                crate::engine::observe(crate::engine::Event::Observation(
                    crate::engine::Observation::VerifyingArtwork {
                        number: tried + position + 1,
                        total,
                    },
                ))
            },
        )
        .await;
        group += 1;
        match result {
            Ok(proofs) => {
                let proved = proofs.len();
                for (check, owner) in checks.drain(..proved).zip(owners.drain(..proved)) {
                    let expected = check.expected;
                    reconciled.pulled[owner].content.artwork = Some(Artwork {
                        cache_id: expected.cache_id,
                        bytes: expected.bytes,
                        sha256: expected.sha256,
                    });
                }
                covers.proven += proved;
                tried += proved;
                failures = 0;
            }
            Err((position, _)) => {
                // That cover stays unproven; the next sync sends it.
                checks.remove(position);
                owners.remove(position);
                tried += 1;
                failures += 1;
            }
        }
    }
    covers.unproven = total - covers.proven;
    Ok(covers)
}

/// A phone track with its audio, as a capture shows it.
struct OnPhone {
    pid: u64,
    path: String,
    artist: String,
    title: String,
    /// What it holds, when the index alone says (not MPEG Layer III).
    identity: Option<TransferredSource>,
    /// Its whole-file SHA-256 and size.
    file: ([u8; 32], u64),
    /// The artwork cache ID the catalogue registers for it.
    cover: Option<u32>,
}

fn on_phone(saved: &snapshot::Snapshot) -> Vec<OnPhone> {
    let covers = saved.catalog.artwork().track_cache_ids();
    saved
        .catalog
        .tracks()
        .iter()
        .filter_map(|t| {
            Some(OnPhone {
                pid: t.sync_id,
                path: t.afc_path.clone()?,
                artist: t.metadata.artist.clone(),
                title: t.metadata.title.clone(),
                identity: phone_identity(&saved.index, t.sync_id),
                file: file_identity(&saved.index, t.sync_id)?,
                cover: covers.get(&t.sync_id).copied(),
            })
        })
        .collect()
}

fn reconcile_saved(
    saved: &snapshot::Snapshot,
    prior: Option<&Prior>,
    library: Option<&Library>,
) -> Reconciled {
    reconcile(&on_phone(saved), matcher(saved), prior, library)
}

/// Whether the capture shows this track ID at this path holding exactly
/// this audio.
fn matcher(saved: &snapshot::Snapshot) -> impl Fn(u64, &str, &TransferredSource) -> bool + '_ {
    |pid, path, source| saved.index.matches_transferred_source(pid, path, source)
}

/// What the phone holds, accounted for. A phone track an earlier binding
/// still describes exactly (`matches`: by track ID, or by audio when the
/// phone numbered it anew) is carried first. Any other is then matched to
/// the one library file holding its audio (an MPEG Layer III file byte for
/// byte, since the index keeps no frame hash), or left unbound.
fn reconcile(
    phone: &[OnPhone],
    matches: impl Fn(u64, &str, &TransferredSource) -> bool,
    prior: Option<&Prior>,
    library: Option<&Library>,
) -> Reconciled {
    let mut by_audio: HashMap<[u8; 32], Vec<(PathBuf, TransferredSource)>> = HashMap::new();
    let mut by_file: HashMap<([u8; 32], u64), Vec<(PathBuf, TransferredSource)>> = HashMap::new();
    for (path, source) in library
        .map_or(&[][..], |l| l.files.as_slice())
        .iter()
        .cloned()
    {
        by_audio
            .entry(source.audio.sha256)
            .or_default()
            .push((path.clone(), source.clone()));
        by_file
            .entry((source.full_file_sha256, source.file_bytes))
            .or_default()
            .push((path, source));
    }
    let bindings = prior.map_or(&[][..], |p| p.bindings.as_slice());
    let mut earlier_pid = HashMap::new();
    let mut earlier_audio = HashMap::new();
    let mut earlier_file = HashMap::new();
    for (n, b) in bindings.iter().enumerate() {
        let source = &b.content.source;
        earlier_pid.insert(b.content.track_pid, n);
        earlier_audio.insert(source.audio.sha256, n);
        earlier_file.insert((source.full_file_sha256, source.file_bytes), n);
    }
    let mut out = Reconciled {
        phone_tracks: phone.len(),
        ..Reconciled::default()
    };
    let mut claimed = BTreeSet::new();
    let mut audio_used = BTreeSet::new();
    // By track ID first, so a second copy of a track's audio never takes
    // its binding.
    let mut found = Vec::new();
    let mut left = Vec::new();
    for track in phone {
        match earlier_pid
            .get(&track.pid)
            .copied()
            .filter(|&n| matches(track.pid, &track.path, &bindings[n].content.source))
        {
            Some(n) => {
                claimed.insert(n);
                found.push((track, n));
            }
            None => left.push(track),
        }
    }
    let mut unmatched = Vec::new();
    for track in left {
        let by_audio = track
            .identity
            .as_ref()
            .and_then(|i| earlier_audio.get(&i.audio.sha256));
        match [by_audio, earlier_file.get(&track.file)]
            .into_iter()
            .flatten()
            .copied()
            .find(|&n| {
                !claimed.contains(&n)
                    && matches(track.pid, &track.path, &bindings[n].content.source)
            }) {
            Some(n) => {
                claimed.insert(n);
                found.push((track, n));
            }
            None => unmatched.push(track),
        }
    }
    for (track, n) in found {
        let (pid, path) = (track.pid, track.path.as_str());
        let binding = &bindings[n];
        let mut content = binding.content.clone();
        content.track_pid = pid;
        content.path = path.to_owned();
        let earlier = content.artwork.take();
        content.artwork = earlier
            .clone()
            .filter(|art| track.cover == Some(art.cache_id));
        // A phone cover no record proves (the phone was cleared and the
        // cover sent again under another cache ID, or an earlier pull lost
        // the record) is read back.
        if content.artwork.is_none()
            && let Some(cache_id) = track.cover
        {
            out.covers.push(CoverCandidate {
                pulled: out.pulled.len(),
                cache_id,
                earlier: earlier.map(|art| (art.bytes, art.sha256)),
            });
        }
        // A recorded source that is gone gives way to the FLAC a conversion
        // was made from, then to the library file holding the audio now;
        // one still there stays, even with other audio, since that is how a
        // changed source is recognised.
        let exists = |path: &PathBuf| path.try_exists().unwrap_or(true);
        let source = binding
            .source
            .clone()
            .filter(exists)
            .or_else(|| {
                let conversion = binding.conversion.as_ref()?;
                Some(conversion.source_path.clone()).filter(exists)
            })
            .or_else(|| {
                unique(by_audio.get(&content.source.audio.sha256), &content.source).map(|(p, _)| p)
            });
        audio_used.insert(content.source.audio.sha256);
        out.carried += 1;
        out.pulled.push(Pulled {
            content,
            source,
            conversion: binding.conversion.clone().map(refreshed),
        });
    }
    for track in unmatched {
        let (pid, path) = (track.pid, track.path.as_str());
        let describe = |reason: &str| PhoneTrack {
            track_id: format!("{pid:016x}"),
            artist: track.artist.clone(),
            title: track.title.clone(),
            copied_to: None,
            reason: Some(reason.into()),
        };
        let candidate = match &track.identity {
            Some(phone) => unique(by_audio.get(&phone.audio.sha256), phone),
            None => by_file.get(&track.file).and_then(|c| match c.as_slice() {
                [one] => Some(one.clone()),
                _ => None,
            }),
        };
        let Some((source, identity)) = candidate.and_then(|(source, from_library)| {
            let identity = track.identity.clone().unwrap_or(from_library);
            matches(pid, path, &identity).then_some((source, identity))
        }) else {
            out.unbound.push(describe(NOT_IN_LIBRARY));
            continue;
        };
        if !audio_used.insert(identity.audio.sha256) {
            out.unbound
                .push(describe("another phone track holds the same audio"));
            continue;
        }
        out.matched += 1;
        if let Some(cache_id) = track.cover {
            out.covers.push(CoverCandidate {
                pulled: out.pulled.len(),
                cache_id,
                earlier: None,
            });
        }
        out.pulled.push(Pulled {
            content: Content {
                source: identity,
                track_pid: pid,
                path: path.to_owned(),
                artwork: None,
                collection: None,
            },
            source: Some(source),
            conversion: None,
        });
    }
    // A source in the library belongs to it: `sync` mirrors it there.
    if let Some(library) = library {
        for pulled in &mut out.pulled {
            if pulled.source.as_deref().is_some_and(|s| library.holds(s)) {
                pulled.content.collection = Some(library.scope);
            }
        }
    }
    out.gone = bindings.len() - claimed.len();
    out
}

/// Why `pull` left a phone track unbound that `pull --tracks` copies.
pub const NOT_IN_LIBRARY: &str = "no library file holds its audio";

/// A phone file's size and whole-file SHA-256, as the capture read them.
fn file_identity(
    index: &crate::engine::state::audio_index::DeviceAudioIndex,
    pid: u64,
) -> Option<([u8; 32], u64)> {
    match (index.tracks().get(&pid), index.other_audio().get(&pid)) {
        (Some(a), _) => Some((a.fingerprint.full_file_sha256, a.fingerprint.file_bytes)),
        (None, Some(a)) => Some((a.full_file_sha256, a.file_bytes)),
        _ => None,
    }
}

/// The library file holding `audio`: the only one, or among several, the
/// only one byte for byte the same.
fn unique(
    candidates: Option<&Vec<(PathBuf, TransferredSource)>>,
    audio: &TransferredSource,
) -> Option<(PathBuf, TransferredSource)> {
    match candidates.map(Vec::as_slice)? {
        [one] => Some(one.clone()),
        many => {
            let mut same = many
                .iter()
                .filter(|(_, s)| s.full_file_sha256 == audio.full_file_sha256);
            match (same.next(), same.next()) {
                (Some(one), None) => Some(one.clone()),
                _ => None,
            }
        }
    }
}

/// What the index alone says a phone file holds: an ALAC's fingerprint or
/// another format's coded audio. An MPEG Layer III file needs its frame
/// hash, which only its bytes give.
fn phone_identity(
    index: &crate::engine::state::audio_index::DeviceAudioIndex,
    pid: u64,
) -> Option<TransferredSource> {
    if let Some(audio) = index.tracks().get(&pid) {
        return Some(audio.fingerprint.clone().into());
    }
    let audio = index.other_audio().get(&pid)?;
    match &audio.codec {
        OtherCodec::Encoded(encoded) => {
            TransferredSource::encoded(audio.file_bytes, audio.full_file_sha256, encoded.clone())
                .ok()
        }
        _ => None,
    }
}

/// A carried conversion, its FLAC's audio identity as this build computes it
/// when the FLAC is still the file that was converted.
fn refreshed(
    mut conversion: crate::engine::state::journal::Conversion,
) -> crate::engine::state::journal::Conversion {
    let current = (|| -> Result<[u8; 32]> {
        let flac =
            music_protocol::media::mpeg::read(&conversion.source_path)?.transferred_identity()?;
        ensure!(
            flac.file_bytes == conversion.source_bytes
                && flac.full_file_sha256 == conversion.source_sha256,
            "the FLAC changed"
        );
        Ok(flac.audio.sha256)
    })();
    if let Ok(audio) = current {
        conversion.source_audio = audio;
    }
    conversion
}

/// Write the reconciled journal at `target`. A journal already there moves
/// to `journal.previous` (replacing an older one) once the new one is
/// complete and has been opened as this build opens any journal.
fn write(
    target: &Path,
    identity: &Identity,
    capture: (&[u8; 32], u32),
    pulled: &[Pulled],
    prior: Option<&Prior>,
    dry_run: bool,
) -> Result<()> {
    let building = sibling(target, "journal.pulling");
    if building.try_exists()? {
        // Only an interrupted pull leaves this; it was never in use.
        fs::remove_dir_all(&building).context("removing an interrupted pull's journal")?;
    }
    let reserved = prior.map(|p| p.reserved.clone()).unwrap_or_default();
    let mut journal = Journal::create(&building, identity)?;
    journal.record_pull(pulled, &reserved, *capture.0, capture.1)?;
    drop(journal);
    drop(Journal::open(&building, identity)?);
    let parent = target.parent().context("journal has no parent directory")?;
    if !dry_run && target.try_exists()? {
        let previous = sibling(target, "journal.previous");
        if previous.try_exists()? {
            fs::remove_dir_all(&previous).context("removing the older previous journal")?;
        }
        fs::rename(target, &previous)?;
    }
    fs::rename(&building, target)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// `pull --tracks`: copy each unbound phone track into `into` as
/// "Artist - Title.ext", check the copy against the capture byte for byte,
/// and bind it to its copy. A name already taken gets " (2)", " (3)", ...;
/// no file is overwritten. Each copy is written beside its final name as
/// ".<track ID>.rustunes-pull", which no selection takes, and linked into
/// place once checked.
async fn copy_unbound(
    reconciled: &mut Reconciled,
    saved: &snapshot::Snapshot,
    into: &Path,
    library: Option<&Library>,
    engine: &crate::engine::Engine,
    seconds: u64,
) -> Result<()> {
    let unbound = std::mem::take(&mut reconciled.unbound);
    let copyable: Vec<_> = unbound
        .iter()
        .filter(|t| t.reason.as_deref() == Some(NOT_IN_LIBRARY))
        .cloned()
        .collect();
    reconciled.unbound = unbound
        .into_iter()
        .filter(|t| t.reason.as_deref() != Some(NOT_IN_LIBRARY))
        .collect();
    if copyable.is_empty() {
        return Ok(());
    }
    let into = fs::canonicalize(into)?;
    let provider = device::provider().await?;
    let mut afc = tokio::time::timeout(
        Duration::from_secs(20),
        music_protocol::protocol::afc::connect_checked(&provider),
    )
    .await
    .context("timed out connecting to the phone's files")??;
    let mut used = BTreeSet::new();
    let total = copyable.len();
    let bytes_total = copyable
        .iter()
        .filter_map(|t| u64::from_str_radix(&t.track_id, 16).ok())
        .filter_map(|pid| file_identity(&saved.index, pid))
        .map(|(_, bytes)| bytes)
        .sum();
    let mut bytes_done = 0;
    for (number, mut track) in copyable.into_iter().enumerate() {
        let pid = u64::from_str_radix(&track.track_id, 16)?;
        let (sha, bytes) =
            file_identity(&saved.index, pid).context("pulled track has no indexed audio")?;
        engine.emit(crate::engine::Event::PullingTrack {
            number: number + 1,
            total,
            track_id: pid,
            title: &track.title,
            artist: &track.artist,
            bytes,
            bytes_done,
            bytes_total,
        })?;
        bytes_done += bytes;
        let control = engine.control(seconds);
        let phone = saved
            .catalog
            .tracks()
            .iter()
            .find(|t| t.sync_id == pid)
            .and_then(|t| t.afc_path.clone())
            .context("pulled track has no file")?;
        let extension = Path::new(&phone)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("m4a")
            .to_ascii_lowercase();
        let staged = into.join(format!(".{pid:016x}.rustunes-pull"));
        if staged.try_exists()? {
            // Only an interrupted pull leaves this name.
            fs::remove_file(&staged)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        let (copied, closed) = copy::read_file(
            &mut afc,
            &phone,
            &mut file,
            bytes,
            copy::CopyLimits {
                maximum_size: bytes,
                maximum_reads: usize::try_from(bytes.div_ceil(copy::CHUNK as u64))? + 1,
            },
            control.deadline,
            Some(control.cancelled.clone()),
            "open phone track read-only",
        )
        .await?;
        copied.map_err(|failure| failure.error)?;
        closed
            .context("phone track close not attempted")?
            .map_err(|failure| failure.error)?;
        file.sync_all()?;
        drop(file);
        let mut hasher = Sha256::new();
        let copied_bytes = std::io::copy(&mut File::open(&staged)?, &mut hasher)?;
        ensure!(
            copied_bytes == bytes && <[u8; 32]>::from(hasher.finalize()) == sha,
            "the copy of {} differs from the phone's file",
            track.title
        );
        let identity = if saved.index.tracks().contains_key(&pid) {
            music_protocol::media::fingerprint::read(&staged).map(Into::into)
        } else {
            music_protocol::media::mpeg::read(&staged).and_then(|s| s.transferred_identity())
        };
        let Some(identity) = identity
            .ok()
            .filter(|i| saved.index.matches_transferred_source(pid, &phone, i))
        else {
            fs::remove_file(&staged)?;
            track.reason = Some("this build cannot read its copy as the phone's track".into());
            reconciled.unbound.push(track);
            continue;
        };
        let destination = free_name(&into, &file_name(&track), &extension, &mut used)?;
        // Linking never replaces a file already there.
        fs::hard_link(&staged, &destination)
            .with_context(|| format!("placing {}", destination.display()))?;
        fs::remove_file(&staged)?;
        fs::set_permissions(
            &destination,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )?;
        File::open(&into)?.sync_all()?;
        reconciled.pulled.push(Pulled {
            content: Content {
                source: identity,
                track_pid: pid,
                path: phone,
                artwork: None,
                collection: library.filter(|l| l.holds(&destination)).map(|l| l.scope),
            },
            source: Some(destination.clone()),
            conversion: None,
        });
        track.copied_to = Some(destination);
        track.reason = None;
        reconciled.copied.push(track);
    }
    engine.emit(crate::engine::Event::TracksPulled { total, bytes_total })?;
    Ok(())
}

/// "Artist - Title", or the title alone, with path separators and control
/// characters replaced, at most 200 bytes.
fn file_name(track: &PhoneTrack) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| if c == '/' || c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .trim_start_matches('.')
            .to_owned()
    };
    let (artist, title) = (clean(&track.artist), clean(&track.title));
    let mut name = match (artist.is_empty(), title.is_empty()) {
        (_, true) => track.track_id.clone(),
        (true, false) => title,
        (false, false) => format!("{artist} - {title}"),
    };
    while name.len() > 200 {
        name.pop();
    }
    name
}

fn free_name(
    directory: &Path,
    name: &str,
    extension: &str,
    used: &mut BTreeSet<PathBuf>,
) -> Result<PathBuf> {
    for n in 1..1000 {
        let candidate = directory.join(match n {
            1 => format!("{name}.{extension}"),
            n => format!("{name} ({n}).{extension}"),
        });
        if !used.contains(&candidate) && !candidate.try_exists()? {
            used.insert(candidate.clone());
            return Ok(candidate);
        }
    }
    bail!("no free name for {name} in {}", directory.display())
}

#[cfg(test)]
mod tests;
