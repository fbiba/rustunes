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

/// Deterministic discovery of explicit file/directory input. Directory input
/// itself may be hidden when deliberately supplied as the root.
/// Children follow the reported policy. No symlink is traversed or accepted.
pub fn select(root: &Path, options: &SelectionOptions) -> Result<Selection> {
    select_impl(root, options)
}

fn select_impl(root: &Path, options: &SelectionOptions) -> Result<Selection> {
    for selector in options.include.iter().chain(&options.exclude) {
        validate_relative(selector)?;
    }
    let metadata = fs::symlink_metadata(root).context("reading selected root metadata")?;
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "selected root must be a regular nonsymlink file or directory"
    );
    let canonical = fs::canonicalize(root).context("resolving selected root")?;
    ensure!(
        same_metadata(&metadata, &fs::symlink_metadata(&canonical)?)?,
        "selected root changed while resolving"
    );
    let mut selection = Selection {
        root: canonical.clone(),
        base_directory: if metadata.is_file() {
            canonical
                .parent()
                .context("source file parent missing")?
                .to_owned()
        } else {
            canonical.clone()
        },
        options: options.clone(),
        files: Vec::new(),
        exclusions: Vec::new(),
        stats: SelectionStats::default(),
    };
    let mut seen = BTreeSet::new();
    if metadata.is_dir() {
        walk(&canonical, "", 0, &mut selection, &mut seen)?;
    } else {
        let name = component(canonical.file_name().context("source filename missing")?)?;
        selection.stats.visited_entries = 1;
        entry(&canonical, &name, 0, false, &mut selection, &mut seen)?;
    }
    for selector in &options.include {
        ensure!(
            seen.iter().any(|path| path == selector)
                || selection
                    .exclusions
                    .iter()
                    .any(|excluded| excluded.descendants_unscanned
                        && within(selector, &excluded.relative_path)),
            "include path was not found in the selected scope: {selector:?}"
        );
    }
    selection
        .files
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    selection
        .exclusions
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(selection)
}

fn walk(
    directory: &Path,
    relative: &str,
    depth: usize,
    selection: &mut Selection,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    ensure!(
        depth <= MAX_DEPTH,
        "source directory depth exceeds {MAX_DEPTH}"
    );
    let before = fs::symlink_metadata(directory)?;
    ensure!(
        before.is_dir() && !before.file_type().is_symlink(),
        "source directory changed or became a symlink"
    );
    selection.stats.visited_directories += 1;
    let mut entries = Vec::new();
    for result in fs::read_dir(directory).context("reading selected source directory")? {
        ensure!(
            selection.stats.visited_entries < MAX_ENTRIES,
            "source discovery exceeds {MAX_ENTRIES} entries"
        );
        let item = result?;
        selection.stats.visited_entries += 1;
        let name = component(&item.file_name())?;
        let relative_path = if relative.is_empty() {
            name
        } else {
            format!("{relative}/{name}")
        };
        validate_relative(&relative_path)?;
        entries.push((relative_path, item.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (relative_path, path) in entries {
        entry(&path, &relative_path, depth + 1, true, selection, seen)?;
    }
    ensure!(
        same_metadata(&before, &fs::symlink_metadata(directory)?)?,
        "source directory changed during discovery"
    );
    Ok(())
}

fn entry(
    path: &Path,
    relative: &str,
    depth: usize,
    child: bool,
    selection: &mut Selection,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    seen.insert(relative.to_owned());
    let metadata = fs::symlink_metadata(path).context("reading source entry metadata")?;
    let directory = metadata.is_dir();
    let options = &selection.options;
    let included = options.include.is_empty()
        || options.include.iter().any(|selector| {
            within(relative, selector) || (directory && within(selector, relative))
        });
    let reason = if metadata.file_type().is_symlink() {
        Some(ExclusionReason::Symlink)
    } else if options
        .exclude
        .iter()
        .any(|selector| within(relative, selector))
    {
        Some(ExclusionReason::ExplicitExclusion)
    } else if !included {
        Some(ExclusionReason::NotIncluded)
    } else if directory && child && relative.rsplit('/').next().unwrap().starts_with('.') {
        Some(ExclusionReason::HiddenDirectory)
    } else if directory && !options.recursive {
        Some(ExclusionReason::NonRecursive)
    } else if directory {
        None
    } else if !metadata.is_file() {
        Some(ExclusionReason::NonRegular)
    } else if !(path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("m4a"))
        || music_protocol::media::mpeg::is_other_audio(path))
    {
        Some(ExclusionReason::UnsupportedExtension)
    } else {
        None
    };
    if let Some(reason) = reason {
        selection.exclusions.push(Exclusion {
            relative_path: relative.to_owned(),
            reason,
            descendants_unscanned: directory,
        });
        *selection.stats.excluded.entry(reason).or_default() += 1;
        return Ok(());
    }
    if directory {
        return walk(path, relative, depth, selection, seen);
    }
    ensure!(
        selection.files.len() < MAX_FILES,
        "source selection exceeds {MAX_FILES} files"
    );
    selection.stats.selected_bytes = selection
        .stats
        .selected_bytes
        .checked_add(metadata.len())
        .context("selected byte count overflow")?;
    ensure!(
        selection.stats.selected_bytes <= MAX_SOURCE_BYTES,
        "source selection exceeds aggregate byte bound"
    );
    selection.stats.selected_files += 1;
    selection.files.push(SelectedSource {
        relative_path: relative.to_owned(),
        absolute_path: path.to_owned(),
        discovered: metadata,
    });
    Ok(())
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

pub mod mp3;

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

/// Inspect every selected source exactly once, without progress reports.
#[cfg(test)]
pub fn inspect(selection: &Selection) -> Result<SourceCollection> {
    inspect_with_progress(selection, |_, _| Ok(()))
}

/// Inspect every selected source exactly once. Unsupported selected audio is a
/// visible per-file failure; excluded extensions never enter this stage. An
/// M4A the ALAC reader refuses is read as another format (AAC, HE-AAC,
/// xHE-AAC); if that fails too, the ALAC reader's refusal is reported.
/// Progress stays separate from the returned complete collection. A callback
/// error stops inspection and cannot produce a complete planning input.
pub fn inspect_with_progress(
    selection: &Selection,
    mut progress: impl FnMut(usize, usize) -> Result<()>,
) -> Result<SourceCollection> {
    let mut tracks = Vec::new();
    let mut mp3_tracks = Vec::new();
    let mut failures = Vec::new();
    let mut retained_text = 0_usize;
    progress(0, selection.files.len())?;
    super::source_reads::warm(selection.files.iter().map(|s| s.absolute_path.as_path()));
    let bytes_total = selection.files.iter().map(|s| s.file_bytes()).sum();
    let mut bytes_done = 0;
    for (index, source) in selection.files.iter().enumerate() {
        crate::engine::observe(crate::engine::Event::Observation(
            crate::engine::Observation::InspectingSource {
                number: index + 1,
                total: selection.files.len(),
                path: &source.relative_path,
                file_bytes: source.file_bytes(),
                bytes_done,
                bytes_total,
            },
        ))?;
        bytes_done += source.file_bytes();
        let inspected = if music_protocol::media::mpeg::is_other_audio(&source.absolute_path) {
            Inspected::Other(inspect_other(source))
        } else {
            match inspect_source(source) {
                Ok(track) => Inspected::Alac(Ok(track)),
                Err(error) => match inspect_other(source) {
                    Ok(other) => Inspected::Other(Ok(other)),
                    Err(other) => Inspected::Alac(Err(refused_m4a(error, &other))),
                },
            }
        };
        match inspected {
            Inspected::Other(Ok(inspected)) => {
                retained_text = retained_text
                    .checked_add(serde_json::to_vec(&inspected)?.len())
                    .context("MP3 metadata size overflow")?;
                ensure!(
                    retained_text <= MAX_TOTAL_TEXT_BYTES,
                    "source collection metadata exceeds text bound"
                );
                mp3_tracks.push(Mp3Track {
                    relative_path: source.relative_path.clone(),
                    source: inspected,
                });
            }
            Inspected::Alac(Ok(track)) => {
                retained_text = retained_text
                    .checked_add(text_bytes(&track.metadata)?)
                    .context("source metadata size overflow")?;
                ensure!(
                    retained_text <= MAX_TOTAL_TEXT_BYTES,
                    "source collection metadata exceeds text bound"
                );
                tracks.push(track);
            }
            Inspected::Alac(Err(error)) | Inspected::Other(Err(error)) => {
                failures.push(InspectionFailure {
                    relative_path: source.relative_path.clone(),
                    message: format!("{error:#}").chars().take(2048).collect(),
                })
            }
        }
        progress(index + 1, selection.files.len())?;
    }
    crate::engine::observe(crate::engine::Event::Observation(
        crate::engine::Observation::SourcesInspected {
            total: selection.files.len(),
            bytes_total,
        },
    ))?;
    let duplicates = duplicate_groups(&tracks);
    Ok(SourceCollection {
        root: selection.root.clone(),
        base_directory: selection.base_directory.clone(),
        options: selection.options.clone(),
        selection_stats: selection.stats.clone(),
        tracks,
        mp3_tracks,
        exclusions: selection.exclusions.clone(),
        failures,
        duplicates,
        flac_to_alac: false,
    })
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

/// A source of any format but ALAC, unchanged since discovery.
fn inspect_other(source: &SelectedSource) -> Result<music_protocol::media::mpeg::Source> {
    ensure!(
        same_metadata(
            &source.discovered,
            &fs::symlink_metadata(&source.absolute_path)?
        )?,
        "selected source changed since discovery"
    );
    let inspected = music_protocol::media::mpeg::read(&source.absolute_path)?;
    ensure!(
        same_metadata(
            &source.discovered,
            &fs::symlink_metadata(&source.absolute_path)?
        )?,
        "selected source changed during inspection"
    );
    Ok(inspected)
}

fn inspect_source(source: &SelectedSource) -> Result<SourceTrack> {
    let before = fs::symlink_metadata(&source.absolute_path)?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "selected source is no longer a regular nonsymlink file"
    );
    ensure!(
        same_metadata(&source.discovered, &before)?,
        "selected source changed since discovery"
    );
    let metadata = metadata::read(&source.absolute_path)?;
    text_bytes(&metadata)?;
    let fingerprint = super::source_reads::fingerprint(&source.absolute_path)?;
    let after = fs::symlink_metadata(&source.absolute_path)?;
    ensure!(
        same_metadata(&before, &after)?,
        "selected source changed between metadata and fingerprint reads"
    );
    ensure!(
        metadata.timing.timescale == fingerprint.audio.timescale
            && metadata.timing.duration_ticks == fingerprint.audio.duration_ticks,
        "metadata and fingerprint timing disagree"
    );
    Ok(SourceTrack {
        relative_path: source.relative_path.clone(),
        fingerprint,
        metadata,
    })
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

use music_protocol::media::held_source::same_observed_file as same_metadata;

#[cfg(test)]
mod tests;
