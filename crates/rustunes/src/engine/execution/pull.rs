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
