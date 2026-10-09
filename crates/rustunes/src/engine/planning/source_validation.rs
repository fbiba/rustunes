//! Local compatibility checks for the implemented sync profile, not a claim
//! that an otherwise valid audio file is unsupported by iOS. Source validation
//! does not establish insertion eligibility against a particular library.

use std::{
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::engine::planning::source_collection::mp3::preparation::PreparedSource;
use music_protocol::media::metadata;

const PROFILE: &str = "alac-source";
const MPEG1_PROFILE: &str = "mpeg1-layer3-insert-batch-source";
const OTHER_PROFILE: &str = "other-format-insert-batch-source";
const DECODE_DEADLINE: Duration = Duration::from_secs(120);
const MAX_DECODE_LOG: usize = 64 * 1024;

#[derive(Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub path: String,
    pub profile: &'static str,
    pub supported_by_current_profile: bool,
    pub audio: Option<Audio>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp3: Option<music_protocol::media::mpeg::Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp3_library_eligibility: Option<&'static str>,
    pub tags: Option<Tags>,
    pub projected_tags: Option<ProjectedTags>,
    pub artwork: Option<Artwork>,
    pub decode: Decode,
    pub issues: Vec<Issue>,
}

#[derive(Serialize)]
pub struct Audio {
    pub codec: &'static str,
    pub file_bytes: u64,
    pub file_sha256: String,
    pub encoded_audio_sha256: String,
    pub sample_rate_hz: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub duration_ticks: u64,
    pub timescale: u32,
    pub packets: u64,
}

#[derive(Serialize)]
pub struct Tags {
    pub title: Vec<String>,
    pub artist: Vec<String>,
    pub album: Vec<String>,
    pub album_artist: Vec<String>,
    pub genre: Vec<String>,
    pub composer: Vec<String>,
    pub copyright: Vec<String>,
    pub release_date: Vec<String>,
    pub compilation: Option<bool>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub content_rating: Option<u8>,
    /// Catalogue comments: MP4 `©cmt`, an ID3 comment without a
    /// description, a Vorbis `COMMENT`.
    pub comment: Vec<String>,
    /// Explicit sort tags.
    pub sort_title: Vec<String>,
    pub sort_artist: Vec<String>,
    pub sort_album: Vec<String>,
    pub sort_album_artist: Vec<String>,
    pub sort_composer: Vec<String>,
}

#[derive(Serialize)]
pub struct ProjectedTags {
    pub policy: &'static str,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub effective_artist: String,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub copyright: Option<String>,
    pub year: Option<u32>,
    pub comment: Option<String>,
    /// The sort names an insert sends: each sort tag, or the name without a
    /// leading article. A group already on the phone keeps its own.
    pub sort_title: String,
    pub sort_artist: String,
    pub sort_album: String,
    pub sort_album_artist: Option<String>,
    pub sort_composer: Option<String>,
}

impl ProjectedTags {
    fn new(policy: &'static str, tags: &metadata::ScalarMetadata) -> Self {
        Self {
            policy,
            title: tags.title.to_owned(),
            artist: tags.artist.to_owned(),
            album: tags.album.to_owned(),
            album_artist: tags.album_artist.map(str::to_owned),
            effective_artist: tags.effective_artist().to_owned(),
            genre: tags.genre.map(str::to_owned),
            composer: tags.composer.map(str::to_owned),
            copyright: tags.copyright.map(str::to_owned),
            year: tags.year,
            comment: tags.comment.map(str::to_owned),
            sort_title: tags.title_sort().to_owned(),
            sort_artist: tags.artist_sort().to_owned(),
            sort_album: tags.album_sort().to_owned(),
            sort_album_artist: tags.album_artist_sort().map(str::to_owned),
            sort_composer: tags.composer_sort().map(str::to_owned),
        }
    }
}

#[derive(Serialize)]
pub struct Artwork {
    /// What sync sends: always a JPEG.
    pub format: &'static str,
    /// The embedded image's format when sync sends a conversion of it (a PNG,
    /// whose JPEG the phone would keep in its place anyway).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<&'static str>,
    pub bytes: usize,
    pub sha256: String,
    pub native_checksum: u32,
    pub image_decode_verified: bool,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decode {
    NotRequested,
    SkippedInvalidInput,
    Passed,
    Failed,
}

#[derive(Serialize)]
pub struct Issue {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

impl Report {
    fn issue(&mut self, severity: Severity, code: &'static str, message: impl Into<String>) {
        self.issues.push(Issue::new(severity, code, message));
    }
}

impl Issue {
    fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
        }
    }
    /// The warning that a source goes without a cover, and why
    /// (`artwork::SourceArtwork::left_out`, `mpeg::cover_left_out`).
    pub(crate) fn cover_left_out(why: &str) -> Self {
        Self::new(
            Severity::Warning,
            "cover_left_out",
            format!(
                "No cover is sent: {why}. Mark one picture as the front cover, or keep only one, to send it."
            ),
        )
    }
}

pub fn inspect(path: &Path, decode: bool) -> Report {
    let mut report = Report {
        schema: "rustunes.validation",
        path: path.to_string_lossy().into_owned(),
        profile: PROFILE,
        supported_by_current_profile: false,
        audio: None,
        mp3: None,
        mp3_library_eligibility: None,
        tags: None,
        projected_tags: None,
        artwork: None,
        decode: if decode {
            Decode::SkippedInvalidInput
        } else {
            Decode::NotRequested
        },
        issues: Vec::new(),
    };
    // An M4A that is not ALAC may be AAC, which the other reader takes.
    let other = music_protocol::media::mpeg::is_other_audio(path)
        || (super::source_reads::fingerprint(path).is_err()
            && music_protocol::media::mpeg::read(path).is_ok());
    if other {
        // Every source this reader takes goes in an insert batch.
        report.profile = if music_protocol::media::mpeg::is_mp3(path) {
            MPEG1_PROFILE
        } else {
            OTHER_PROFILE
        };
        report.mp3_library_eligibility = Some("not_checked");
        match music_protocol::media::mpeg::read(path) {
            Ok(source) => {
                report.profile = if source.stream.is_none() {
                    MPEG1_PROFILE
                } else {
                    OTHER_PROFILE
                };
                let label = source.format().label();
                report.tags = Some(other_tags(&source.metadata));
                if let Ok(Some(cover)) =
                    crate::engine::planning::source_collection::mp3::artwork::batch_cover(
                        path, &source,
                    )
                {
                    report.artwork = Some(Artwork {
                        format: "jpeg",
                        converted_from: music_protocol::media::mpeg::cover_picture(&source.artwork)
                            .and_then(|p| match p.mime {
                                "image/png" => Some("png"),
                                "image/gif" => Some("gif"),
                                "image/bmp" => Some("bmp"),
                                "image/tiff" => Some("tiff"),
                                _ => None,
                            }),
                        bytes: cover.bytes.len(),
                        sha256: hex(&cover.sha256),
                        native_checksum: cover.checksum,
                        image_decode_verified: false,
                    });
                }
                if let Some(why) = music_protocol::media::mpeg::cover_left_out(&source.artwork) {
                    report.issues.push(Issue::cover_left_out(&why));
                }
                // Reuse the same source predicate as planning/apply. This
                // stable reread constructs no SQL proof or native capability.
                match PreparedSource::read_expected(
                    path, &source,
                )
                .and_then(|prepared| {
                    prepared.ensure_batch_insertion_allowed()?;
                    if let Ok(projected) = prepared.expected().and_then(|e| e.projected()) {
                        if let Some(tags) = &mut report.tags {
                            tags.year = projected.year;
                        }
                        report.projected_tags =
                            Some(ProjectedTags::new("insert_batch_projection", &projected));
                    }
                    Ok(())
                }) {
                    Ok(()) => report.issue(
                        Severity::Warning,
                        "mp3_library_eligibility_not_checked",
                        format!("Insert-batch {label} source checks passed: sync adds it in an insert batch, with its cover if it has one. Run plan or sync --dry-run against a complete snapshot to check existing matches and journal state; fresh library and allocation checks are required before transfer."),
                    ),
                    Err(error) => report.issue(
                        Severity::Error,
                        "mp3_source_profile_unsupported",
                        format!("{error:#}"),
                    ),
                }
                if decode {
                    match decode_audio(path, DECODE_DEADLINE).and_then(|()| {
                        ensure!(
                            music_protocol::media::mpeg::read(path)? == source,
                            "source changed during decode"
                        );
                        Ok(())
                    }) {
                        Ok(()) => report.decode = Decode::Passed,
                        Err(error) => {
                            report.decode = Decode::Failed;
                            report.issue(Severity::Error, "decode_failed", format!("{error:#}"));
                        }
                    }
                }
                report.mp3 = Some(source);
            }
            Err(error) => report.issue(
                Severity::Error,
                "source_inspection_failed",
                format!("{error:#}"),
            ),
        }
        report.supported_by_current_profile = !report
            .issues
            .iter()
            .any(|issue| issue.severity == Severity::Error);
        return report;
    }
    if let Err(error) = inspect_inner(path, decode, &mut report) {
        report.issue(
            Severity::Error,
            "source_inspection_failed",
            format!("{error:#}"),
        );
    }
    report.supported_by_current_profile = !report
        .issues
        .iter()
        .any(|issue| issue.severity == Severity::Error);
    report
}

fn inspect_inner(path: &Path, decode: bool, report: &mut Report) -> Result<()> {
    ensure!(
        path.to_str().is_some(),
        "source path must be UTF-8 for the CLI report"
    );
    let before = fs::symlink_metadata(path).context("reading source attributes")?;
    ensure!(
        before.file_type().is_file(),
        "source must be a regular nonsymlink file"
    );
    let fingerprint = super::source_reads::fingerprint(path)
        .map_err(|error| match music_protocol::media::mpeg::read(path) {
            Err(other) => crate::engine::planning::source_collection::refused_m4a(error, &other),
            Ok(_) => error,
        })
        .context("checking ALAC container and encoded audio identity")?;
    let tags = metadata::read(path).context("reading standard MP4 metadata")?;
    check_tags(&tags, report);
    if let Ok(projected) = tags.projected() {
        report.projected_tags = Some(ProjectedTags::new("first_standard_data_value", &projected));
    }
    report.audio = Some(Audio {
        codec: "ALAC",
        file_bytes: fingerprint.file_bytes,
        file_sha256: hex(&fingerprint.full_file_sha256),
        encoded_audio_sha256: hex(&fingerprint.audio.sha256),
        sample_rate_hz: tags.sample_rate,
        bit_depth: tags.bit_depth,
        channels: tags.channels,
        duration_ticks: tags.timing.duration_ticks,
        timescale: tags.timing.timescale,
        packets: tags.timing.packets,
    });
    report.tags = Some(Tags {
        title: tags.title,
        artist: tags.artist,
        album: tags.album,
        album_artist: tags.album_artist,
        genre: tags.genre,
        composer: tags.composer,
        copyright: tags.copyright,
        release_date: tags.release_date,
        compilation: tags.compilation,
        year: tags.year,
        track: tags.track,
        track_total: tags.track_total,
        disc: tags.disc,
        disc_total: tags.disc_total,
        content_rating: tags.content_rating,
        comment: tags.comment,
        sort_title: tags.sort_title,
        sort_artist: tags.sort_artist,
        sort_album: tags.sort_album,
        sort_album_artist: tags.sort_album_artist,
        sort_composer: tags.sort_composer,
    });
    match super::source_reads::artwork(path) {
        Ok(artwork) => {
            ensure!(
                artwork.source_size == fingerprint.file_bytes
                    && artwork.source_sha256 == fingerprint.full_file_sha256,
                "source changed between audio and artwork inspection"
            );
            if let Some(cover) = artwork.cover {
                report.artwork = Some(Artwork {
                    format: "jpeg",
                    converted_from: artwork.embedded.as_ref().map(|image| image.format),
                    bytes: cover.bytes.len(),
                    sha256: hex(&cover.sha256),
                    native_checksum: cover.checksum,
                    image_decode_verified: false,
                });
            } else if let Some(why) = &artwork.left_out {
                report.issues.push(Issue::cover_left_out(why));
            } else {
                report.issue(
                    Severity::Warning,
                    "artwork_absent",
                    "No embedded cover; stock Music will have no source artwork to display.",
                );
            }
        }
        Err(error) => report.issue(
            Severity::Error,
            "artwork_profile_unsupported",
            format!("The file's cover images cannot be read: {error:#}"),
        ),
    }
    if decode {
        match decode_audio(path, DECODE_DEADLINE) {
            Ok(()) => report.decode = Decode::Passed,
            Err(error) => {
                report.decode = Decode::Failed;
                report.issue(Severity::Error, "audio_decode_failed", format!("{error:#}"));
            }
        }
    }
    let after = fs::symlink_metadata(path)?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && after.file_type().is_file(),
        "source changed during validation"
    );
    Ok(())
}

fn check_tags(source: &metadata::Metadata, report: &mut Report) {
    if let Err(error) = source.projected() {
        report.issue(
            Severity::Error,
            "metadata_projection_unsupported",
            format!("{error:#}"),
        );
    }
    if let Err(error) = source.ensure_compilation_profile() {
        report.issue(
            Severity::Error,
            "compilation_grouping_unverified",
            format!(
                "{error:#}. Existing album compatibility must also pass fresh collection planning."
            ),
        );
    }
    if let Err(error) = music_protocol::media::format::Format::Alac.check_phone_limits(
        source.sample_rate,
        source.channels,
        source.bit_depth,
        0,
    ) {
        report.issue(
            Severity::Error,
            "audio_profile_unsupported",
            format!("{error:#}"),
        );
    }
    for (name, values, required) in [
        ("title", &source.title, true),
        ("artist", &source.artist, true),
        ("album", &source.album, true),
        ("album_artist", &source.album_artist, false),
        ("genre", &source.genre, false),
        ("composer", &source.composer, false),
        ("copyright", &source.copyright, false),
    ] {
        // An empty value is no value (`metadata::tag_value`): a field left
        // with none is absent.
        let values: Vec<_> = metadata::tag_values(values).collect();
        if required && values.is_empty() {
            report.issue(
                Severity::Error,
                "required_tag_absent",
                format!("The current insertion profile requires a standard {name} tag."),
            );
        }
        if values.len() > 1 {
            report.issue(Severity::Warning, "multiple_tag_values", format!("{name} contains {} ordered values. Inspection preserves them; the catalog projection uses the first standard data value when the atom layout is unambiguous.", values.len()));
        }
        if values
            .iter()
            .any(|value| value.trim().is_empty() || value.contains('\0'))
        {
            report.issue(
                Severity::Error,
                "invalid_tag_text",
                format!("{name} contains blank text or NUL characters."),
            );
        }
    }
    for (name, values) in [
        ("sort title", &source.sort_title),
        ("sort artist", &source.sort_artist),
        ("sort album", &source.sort_album),
        ("sort album artist", &source.sort_album_artist),
        ("sort composer", &source.sort_composer),
    ] {
        let count = metadata::tag_values(values).count();
        if count > 1 {
            report.issue(Severity::Warning, "multiple_tag_values", format!("{name} contains {count} ordered values. Inspection preserves them; the catalog projection uses the first."));
        }
    }
    for (name, number, total) in [
        ("track", source.track, source.track_total),
        ("disc", source.disc, source.disc_total),
    ] {
        if let Some(total) = total
            && number.is_none_or(|number| number > total)
        {
            report.issue(
                Severity::Error,
                "invalid_numbering",
                format!("{name} total requires a number between one and the total."),
            );
        }
    }
}

/// Run FFmpeg directly, never through a shell. Input has already passed the
/// self-contained single-track check. Bound runtime and captured diagnostics.
fn decode_audio(path: &Path, deadline: Duration) -> Result<()> {
    let absolute = fs::canonicalize(path)?;
    let mut child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-v",
            "error",
            "-xerror",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(absolute)
        .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ffmpeg; install FFmpeg or omit --decode")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing FFmpeg diagnostics")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = sender.send(bounded_log(stderr));
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result.context("waiting for FFmpeg")?;
                anyhow::bail!("FFmpeg decode exceeded {} seconds", deadline.as_secs());
            }
        }
    };
    let (log, truncated) = receiver
        .recv_timeout(Duration::from_secs(2))
        .context("closing FFmpeg diagnostics")??;
    ensure!(
        status.success() && log.is_empty() && !truncated,
        "FFmpeg decode did not complete cleanly (status {status}): {}{}",
        String::from_utf8_lossy(&log),
        if truncated {
            " [diagnostics truncated]"
        } else {
            ""
        }
    );
    Ok(())
}

fn bounded_log(mut reader: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let take = count.min(MAX_DECODE_LOG - captured.len());
        captured.extend_from_slice(&buffer[..take]);
        truncated |= take < count;
    }
    Ok((captured, truncated))
}

/// The tags of a source the other reader took, as the ALAC report lists them.
/// The year comes from the insert projection, which reads it from the date.
fn other_tags(tags: &music_protocol::media::mpeg::Tags) -> Tags {
    Tags {
        title: tags.title.clone(),
        artist: tags.artist.clone(),
        album: tags.album.clone(),
        album_artist: tags.album_artist.clone(),
        genre: tags.genre.clone(),
        composer: tags.composer.clone(),
        copyright: tags.copyright.clone(),
        release_date: tags.release_date.clone(),
        compilation: tags.compilation,
        year: None,
        track: tags.track,
        track_total: tags.track_total,
        disc: tags.disc,
        disc_total: tags.disc_total,
        content_rating: None,
        comment: tags
            .catalogue_comments()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        sort_title: tags.sort_title.clone(),
        sort_artist: tags.sort_artist.clone(),
        sort_album: tags.sort_album.clone(),
        sort_album_artist: tags.sort_album_artist.clone(),
        sort_composer: tags.sort_composer.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut result, byte| {
            write!(result, "{byte:02x}").expect("writing to String cannot fail");
            result
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        crate::test_fixtures::workspace_root()
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn reports_supported_container_without_claiming_audio_or_image_decode() {
        let report = inspect(&fixture("silence-alac.m4a"), false);
        assert!(report.supported_by_current_profile);
        assert!(report.decode == Decode::NotRequested);
        assert_eq!(report.audio.as_ref().unwrap().sample_rate_hz, 44_100);
        assert_eq!(report.tags.as_ref().unwrap().artist, ["First artist"]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["decode"], "not_requested");
    }

    #[test]
    fn reads_aac_as_another_format_and_does_not_run_decoder_for_invalid_input() {
        // An AAC M4A goes by the other formats' reader; this one has no title.
        let report = inspect(&fixture("silence-aac.m4a"), false);
        assert_eq!(report.profile, "other-format-insert-batch-source");
        assert!(report.mp3.is_some() && report.audio.is_none());
        assert!(!report.supported_by_current_profile);
        assert!(report.issues.iter().any(|i| {
            i.code == "mp3_source_profile_unsupported" && i.message.contains("missing title")
        }));
        let missing = inspect(&fixture("missing.m4a"), true);
        assert!(!missing.supported_by_current_profile);
        assert!(missing.decode == Decode::SkippedInvalidInput);
        assert_eq!(missing.issues[0].code, "source_inspection_failed");
    }

    #[test]
    fn reports_all_mapping_conflicts_without_discarding_repeated_values() {
        let mut tags = metadata::read(&fixture("silence-alac.m4a")).unwrap();
        tags.artist = vec!["One, Two".into(), "One".into(), "Two".into()];
        tags.album.clear();
        tags.track = Some(11);
        tags.track_total = Some(10);
        // Any rate or depth the phone takes passes; six channels do not.
        tags.sample_rate = 96_000;
        tags.bit_depth = 24;
        tags.channels = 6;
        let mut report = inspect(&fixture("silence-alac.m4a"), false);
        report.issues.clear();
        check_tags(&tags, &mut report);
        for code in [
            "multiple_tag_values",
            "required_tag_absent",
            "invalid_numbering",
            "audio_profile_unsupported",
        ] {
            assert!(
                report.issues.iter().any(|issue| issue.code == code),
                "missing {code}"
            );
        }
        assert_eq!(tags.artist.len(), 3);
    }

    #[test]
    fn compilation_profile_requires_one_explicit_album_artist() {
        let mut source = metadata::read(&fixture("silence-alac.m4a")).unwrap();
        source.compilation = Some(true);
        source.album_artist = vec!["CLI Compilation Σύνολο".into()];
        let mut report = inspect(&fixture("silence-alac.m4a"), false);
        check_tags(&source, &mut report);
        assert!(!report.issues.iter().any(|i| i.severity == Severity::Error));
        for values in [
            vec![],
            vec!["".into()],
            vec![" \t".into()],
            vec!["One".into(), "Two".into()],
        ] {
            source.album_artist = values;
            report.issues.clear();
            check_tags(&source, &mut report);
            assert!(
                report
                    .issues
                    .iter()
                    .any(|i| i.code == "compilation_grouping_unverified")
            );
        }
        for flag in [None, Some(false)] {
            source.compilation = flag;
            source.album_artist.clear();
            report.issues.clear();
            check_tags(&source, &mut report);
            assert!(!report.issues.iter().any(|i| i.severity == Severity::Error));
        }
    }

    #[test]
    fn an_empty_tag_value_is_checked_as_an_absent_tag() {
        let issues = |edit: &dyn Fn(&mut metadata::Metadata)| {
            let mut tags = metadata::read(&fixture("silence-alac.m4a")).unwrap();
            edit(&mut tags);
            let mut report = inspect(&fixture("silence-alac.m4a"), false);
            report.issues.clear();
            check_tags(&tags, &mut report);
            report
                .issues
                .into_iter()
                .map(|issue| (issue.code, issue.message))
                .collect::<Vec<_>>()
        };
        // Emptied optional tags are checked as removed ones, and an empty
        // value beside one that says something leaves it single.
        let removed = issues(&|t| {
            t.genre.clear();
            t.composer.clear();
            t.sort_composer.clear();
        });
        assert_eq!(removed, []);
        let emptied = issues(&|t| {
            t.genre = vec![String::new()];
            t.composer = vec![String::new(), String::new()];
            t.sort_composer = vec![String::new(), "B, Artist".into()];
            t.artist.insert(0, String::new());
        });
        assert_eq!(emptied, removed);
        // A lone empty title is a missing one.
        let missing = issues(&|t| t.title.clear());
        assert!(
            missing
                .iter()
                .any(|(code, _)| *code == "required_tag_absent")
        );
        assert_eq!(issues(&|t| t.title = vec![String::new()]), missing);
        // Blank text says something, and is still refused.
        assert_eq!(
            issues(&|t| t.composer = vec![" ".into()]),
            [(
                "invalid_tag_text",
                "composer contains blank text or NUL characters.".to_owned()
            )]
        );
    }

    #[test]
    fn diagnostics_are_drained_but_bounded() {
        let (log, truncated) = bounded_log(&vec![b'x'; MAX_DECODE_LOG + 10][..]).unwrap();
        assert_eq!(log.len(), MAX_DECODE_LOG);
        assert!(truncated);
        let (empty, truncated) = bounded_log(&b""[..]).unwrap();
        assert!(empty.is_empty() && !truncated);
    }
}

#[cfg(test)]
pub(crate) mod mpeg_tests;
