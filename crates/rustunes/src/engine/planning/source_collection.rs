//! Bounded offline source selection and inspection. Discovery does not hash
//! audio. Inspection retains standard tag arrays and reports every file error
//! and audio-duplicate group; neither selection nor matching grants ownership.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, Metadata as FileMetadata},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use music_protocol::media::fingerprint;
use music_protocol::media::metadata;

const MAX_ENTRIES: usize = 10_000;
const MAX_FILES: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_TEXT_VALUE_BYTES: usize = 1024 * 1024;
const MAX_TEXT_VALUES: usize = 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SelectionOptions {
    pub recursive: bool,
    /// Exact relative file paths or directory subtrees; empty means all.
    pub include: BTreeSet<String>,
    /// Exact relative file paths or directory subtrees. Exclusions win.
    pub exclude: BTreeSet<String>,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExclusionReason {
    ExplicitExclusion,
    NotIncluded,
    HiddenDirectory,
    Symlink,
    UnsupportedExtension,
    NonRegular,
    NonRecursive,
}

impl ExclusionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitExclusion => "explicit-exclusion",
            Self::NotIncluded => "not-included",
            Self::HiddenDirectory => "hidden-directory",
            Self::Symlink => "symlink",
            Self::UnsupportedExtension => "unsupported-extension",
            Self::NonRegular => "nonregular",
            Self::NonRecursive => "nonrecursive",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Exclusion {
    pub relative_path: String,
    pub reason: ExclusionReason,
    /// No descendant count is invented for a skipped directory.
    pub descendants_unscanned: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SelectionStats {
    pub visited_entries: usize,
    pub visited_directories: usize,
    pub selected_files: usize,
    pub selected_bytes: u64,
    pub excluded: BTreeMap<ExclusionReason, usize>,
}

pub struct SelectedSource {
    relative_path: String,
    absolute_path: PathBuf,
    discovered: FileMetadata,
}

impl SelectedSource {
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
    pub fn path(&self) -> &Path {
        &self.absolute_path
    }
    pub fn file_bytes(&self) -> u64 {
        self.discovered.len()
    }
}

pub struct Selection {
    root: PathBuf,
    base_directory: PathBuf,
    options: SelectionOptions,
    files: Vec<SelectedSource>,
    exclusions: Vec<Exclusion>,
    stats: SelectionStats,
}

impl Selection {
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Base for relative paths; explicit file input uses its parent directory.
    pub fn base_directory(&self) -> &Path {
        &self.base_directory
    }
    pub fn options(&self) -> &SelectionOptions {
        &self.options
    }
    pub fn files(&self) -> &[SelectedSource] {
        &self.files
    }
    pub fn exclusions(&self) -> &[Exclusion] {
        &self.exclusions
    }
    pub fn stats(&self) -> &SelectionStats {
        &self.stats
    }
}

/// Whether a selection of the directory `root` with `options` would reach a
/// regular file at the absolute `path`, were one there: the same rules as
/// discovery, applied to the path alone.
pub fn reaches(root: &Path, options: &SelectionOptions, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(parts) = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .filter(|parts| !parts.is_empty())
    else {
        return false;
    };
    let relative = parts.join("/");
    let directories = &parts[..parts.len() - 1];
    (options.recursive || directories.is_empty())
        && directories.iter().all(|name| !name.starts_with('.'))
        && !options
            .exclude
            .iter()
            .any(|selector| within(&relative, selector))
        && (options.include.is_empty()
            || options
                .include
                .iter()
                .any(|selector| within(&relative, selector)))
}

fn within(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn component(name: &std::ffi::OsStr) -> Result<String> {
    let name = name.to_str().context("source path is not UTF-8")?;
    ensure!(
        !name.contains('/'),
        "source filename contains a path separator"
    );
    validate_relative(name)?;
    Ok(name.to_owned())
}

fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH_BYTES && !path.contains(['\\', '\0']),
        "invalid relative source path"
    );
    ensure!(
        path.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "source path must be relative without traversal"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_))),
        "source path is not relative"
    );
    Ok(())
}

pub struct Mp3Track {
    pub relative_path: String,
    pub source: music_protocol::media::mpeg::Source,
}

pub struct SourceTrack {
    pub relative_path: String,
    pub fingerprint: fingerprint::SourceFingerprint,
    /// Original standard arrays, including absent/multiple values; no guessed join.
    pub metadata: metadata::Metadata,
}

#[derive(Debug)]
pub struct InspectionFailure {
    pub relative_path: String,
    pub message: String,
}

pub struct DuplicateGroup {
    pub encoded_alac_sha256: [u8; 32],
    pub relative_paths: Vec<String>,
    pub identical_complete_files: bool,
    pub identical_standard_metadata: bool,
}

pub struct SourceCollection {
    pub root: PathBuf,
    pub base_directory: PathBuf,
    pub options: SelectionOptions,
    pub selection_stats: SelectionStats,
    pub tracks: Vec<SourceTrack>,
    pub mp3_tracks: Vec<Mp3Track>,
    pub exclusions: Vec<Exclusion>,
    pub failures: Vec<InspectionFailure>,
    /// All aliases/variants remain in tracks. The planner must resolve groups.
    pub duplicates: Vec<DuplicateGroup>,
    /// `--flac-to-alac`: a FLAC not yet on the phone is sent as an ALAC
    /// converted from it. Inspection leaves it off; the request sets it.
    pub flac_to_alac: bool,
}

impl SourceCollection {
    pub fn inspected_len(&self) -> usize {
        self.tracks.len() + self.mp3_tracks.len()
    }
    pub fn inspected_bytes(&self) -> Result<u64> {
        self.tracks
            .iter()
            .map(|t| t.fingerprint.file_bytes)
            .chain(
                self.mp3_tracks
                    .iter()
                    .map(|t| t.source.fingerprint.file_bytes),
            )
            .try_fold(0u64, |a, b| {
                a.checked_add(b).context("source byte count overflow")
            })
    }
    pub fn mp3_duplicates(&self) -> Vec<Vec<String>> {
        let mut groups = BTreeMap::<&str, Vec<String>>::new();
        for t in &self.mp3_tracks {
            groups
                .entry(&t.source.fingerprint.audio.sha256)
                .or_default()
                .push(t.relative_path.clone());
        }
        groups.into_values().filter(|g| g.len() > 1).collect()
    }
    pub fn ensure_complete(&self) -> Result<()> {
        ensure!(
            self.failures.is_empty(),
            "{} selected source files could not be inspected",
            self.failures.len()
        );
        Ok(())
    }

    pub fn ensure_unambiguous(&self) -> Result<()> {
        self.ensure_complete()?;
        ensure!(
            self.duplicates.is_empty() && self.mp3_duplicates().is_empty(),
            "{} encoded-audio duplicate groups require explicit selection",
            self.duplicates.len() + self.mp3_duplicates().len()
        );
        Ok(())
    }
}

/// Why neither reader takes an M4A: an AAC file's real fault would otherwise
/// hide behind the ALAC reader's.
pub(crate) fn refused_m4a(alac: anyhow::Error, other: &anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("not ALAC ({alac:#}), nor AAC ({other:#})")
}

/// Which reader took a selected source, and what it found.
enum Inspected {
    Alac(Result<SourceTrack>),
    Other(Result<music_protocol::media::mpeg::Source>),
}

fn text_bytes(metadata: &metadata::Metadata) -> Result<usize> {
    let mut total = 0;
    for values in [
        &metadata.title,
        &metadata.artist,
        &metadata.album,
        &metadata.album_artist,
        &metadata.genre,
        &metadata.composer,
        &metadata.copyright,
        &metadata.release_date,
    ] {
        ensure!(
            values.len() <= MAX_TEXT_VALUES,
            "too many repeated standard metadata values"
        );
        for value in values {
            ensure!(
                value.len() <= MAX_TEXT_VALUE_BYTES,
                "standard metadata value exceeds text bound"
            );
            total += value.len();
            ensure!(
                total <= MAX_TOTAL_TEXT_BYTES,
                "track metadata exceeds text bound"
            );
        }
    }
    Ok(total)
}

fn duplicate_groups(tracks: &[SourceTrack]) -> Vec<DuplicateGroup> {
    let mut groups: BTreeMap<[u8; 32], Vec<&SourceTrack>> = BTreeMap::new();
    for track in tracks {
        groups
            .entry(track.fingerprint.audio.sha256)
            .or_default()
            .push(track);
    }
    let mut duplicates: Vec<_> = groups
        .into_iter()
        .filter(|(_, group)| group.len() > 1)
        .map(|(hash, group)| {
            let first = group[0];
            DuplicateGroup {
                encoded_alac_sha256: hash,
                relative_paths: group
                    .iter()
                    .map(|track| track.relative_path.clone())
                    .collect(),
                identical_complete_files: group.iter().all(|track| {
                    track.fingerprint.full_file_sha256 == first.fingerprint.full_file_sha256
                }),
                identical_standard_metadata: group
                    .iter()
                    .all(|track| track.metadata == first.metadata),
            }
        })
        .collect();
    duplicates.sort_by(|a, b| a.relative_paths.cmp(&b.relative_paths));
    duplicates
}
