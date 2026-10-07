//! Source-derived MPEG insertion preparation. This module has no capability,
//! journal transition, destination allocator, or transport. Its coded-duration
//! proposal is explicitly separate from receiver-qualified native timing.
//! What a source projects into catalogue fields, its coded timing and the
//! operation bytes they make come from music-protocol; this module decides
//! which sources may be inserted, and how.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use music_protocol::media::fingerprint::TransferredSource;
use music_protocol::media::mpeg::Source;
pub use music_protocol::media::mpeg::projection::{CodedTimingProposal, Metadata};
use music_protocol::media::mpeg::projection::{coded_timing, project, unprojected_frames};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompatibilityIssue {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArtworkAssessment {
    pub embedded_picture_count: usize,
    pub front_cover_count: usize,
    pub embedded_bytes_preserved: bool,
    pub native_artwork_action_qualified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Assessment {
    pub schema: &'static str,
    pub source_file_sha256: String,
    pub encoded_audio_sha256: String,
    pub metadata: Option<Metadata>,
    pub coded_timing_proposal: Option<CodedTimingProposal>,
    pub artwork: ArtworkAssessment,
    /// Source shape/policy issues that need handling in addition to receiver
    /// qualification. They never turn an exact existing Keep into a conflict.
    pub compatibility_issues: Vec<CompatibilityIssue>,
    pub required_qualification: Vec<&'static str>,
    pub execution_authorized: bool,
    /// How a planned addition inserts it: `insert_batch`, the route every MP3
    /// addition takes. Set by planning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insertion_route: Option<&'static str>,
}

/// Issues an insert batch handles: its receiver checks cover every projected
/// tag and reused groups, it sends one front cover with the audio, an MP3's
/// gapless fields and every format other than MPEG-1 Layer III. Tag fields
/// it does not project (a described comment such as iTunNORM, an ID3v1 tag)
/// stay in the file it sends.
const BATCH_HANDLED: [&str; 6] = [
    "mp3_extended_metadata_mapping_required",
    "mp3_artwork_transfer_unqualified",
    "mp3_gapless_batch_only",
    "mp3_unmapped_metadata_frames",
    "mp3_trailing_tag_not_projected",
    "mp3_other_format_batch_only",
];

impl Assessment {
    /// The issues that keep the source out of an insert batch.
    pub(crate) fn batch_refusals(&self) -> impl Iterator<Item = &CompatibilityIssue> {
        self.compatibility_issues
            .iter()
            .filter(|issue| !BATCH_HANDLED.contains(&issue.code))
    }
}

fn issue(issues: &mut Vec<CompatibilityIssue>, code: &'static str, detail: impl Into<String>) {
    issues.push(CompatibilityIssue {
        code,
        detail: detail.into(),
    });
}

/// Descriptive analysis only. `PreparedSource` rereads the exact stable
/// source before the insert batch takes its fields (`operation_with_metadata`).
pub fn assess(source: &Source) -> Assessment {
    let mut issues = Vec::new();
    if source.stream.is_some() {
        issue(
            &mut issues,
            "mp3_other_format_batch_only",
            format!("{} goes only in an insert batch", source.format().label()),
        );
    }
    if source.fingerprint.file_bytes > music_protocol::protocol::transfer::MAX_AUDIO {
        issue(
            &mut issues,
            "mp3_native_transfer_size_limit",
            format!(
                "Source has {} bytes; the existing native audio transfer limit is {} bytes (64 MiB)",
                source.fingerprint.file_bytes,
                music_protocol::protocol::transfer::MAX_AUDIO
            ),
        );
    }
    let metadata = match project(source) {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            issue(
                &mut issues,
                "mp3_metadata_policy_required",
                error.to_string(),
            );
            None
        }
    };
    if metadata.as_ref().is_some_and(|m| {
        m.album_artist.is_some()
            || m.genre.is_some()
            || m.composer.is_some()
            || m.copyright.is_some()
            || m.year.is_some()
            || m.track.is_some()
            || m.track_total.is_some()
            || m.disc.is_some()
            || m.disc_total.is_some()
            || m.compilation.is_some()
            || m.comment.is_some()
            || m.sort_title.is_some()
            || m.sort_artist.is_some()
            || m.sort_album.is_some()
            || m.sort_album_artist.is_some()
            || m.sort_composer.is_some()
    }) {
        issue(
            &mut issues,
            "mp3_extended_metadata_mapping_required",
            "Additional tags are projected without loss; they go only in an insert batch",
        );
    }
    let extra_frames = unprojected_frames(source);
    if !extra_frames.is_empty() {
        issue(
            &mut issues,
            "mp3_unmapped_metadata_frames",
            format!(
                "Tag fields stay in the file but do not reach the phone's catalogue: {}",
                extra_frames.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    if source.trailing_tag_bytes != 0 {
        issue(
            &mut issues,
            "mp3_trailing_tag_not_projected",
            "A trailing ID3v1 tag stays in the file; its fields are not projected",
        );
    }
    if !source.artwork.is_empty() {
        issue(
            &mut issues,
            "mp3_artwork_transfer_unqualified",
            "Embedded pictures are inspected; the cover goes only in an insert batch",
        );
    }
    let coded_timing_proposal = match coded_timing(source) {
        Ok(timing) => Some(timing),
        Err(error) => {
            issue(&mut issues, "mp3_timing_policy_required", error.to_string());
            None
        }
    };
    if coded_timing_proposal.is_some() && source.stream.is_none() {
        issue(
            &mut issues,
            "mp3_gapless_batch_only",
            "The encoder delay, drain and played duration Apple's sync finds for an MP3 go only in an insert batch",
        );
    }
    Assessment {
        schema: "rustunes.mpeg-source-preparation",
        source_file_sha256: source.fingerprint.full_file_sha256.clone(),
        encoded_audio_sha256: source.fingerprint.audio.sha256.clone(),
        metadata,
        coded_timing_proposal,
        artwork: ArtworkAssessment {
            embedded_picture_count: source.artwork.len(),
            front_cover_count: source
                .artwork
                .iter()
                .filter(|p| p.picture_type == 3)
                .count(),
            embedded_bytes_preserved: true,
            native_artwork_action_qualified: false,
        },
        compatibility_issues: issues,
        required_qualification: vec![
            "mpeg_positive_timing_receiver",
            "mpeg_source_metadata_and_collection_mapping",
            "mpeg_public_insertion_completion_and_recovery",
        ],
        execution_authorized: false,
        insertion_route: None,
    }
}

/// Created only by a fresh stable read that equals the selected inspection.
/// This is source preparation, never permission to reserve, send or complete.
pub struct PreparedSource {
    source: Source,
    identity: TransferredSource,
    assessment: Assessment,
}

impl PreparedSource {
    pub fn read_expected(path: &Path, expected: &Source) -> Result<Self> {
        ensure!(
            expected.fingerprint.file_bytes <= music_protocol::protocol::transfer::MAX_AUDIO,
            "MPEG source exceeds the existing 64 MiB native audio transfer limit"
        );
        let source = music_protocol::media::mpeg::read(path)?;
        ensure!(&source == expected, "MPEG source changed after selection");
        let identity = source.transferred_identity()?;
        let assessment = assess(&source);
        Ok(Self {
            source,
            identity,
            assessment,
        })
    }
    pub fn source(&self) -> &Source {
        &self.source
    }
    pub fn identity(&self) -> &TransferredSource {
        &self.identity
    }
    pub fn assessment(&self) -> &Assessment {
        &self.assessment
    }
    /// Input to the ordinary staged pipeline. Rich scalar metadata and embedded
    /// APIC are preserved, but neither grants a native metadata/artwork permit.
    pub(crate) fn coded_insert_fields(&self) -> Result<(&Metadata, &CodedTimingProposal)> {
        ensure!(
            self.assessment
                .compatibility_issues
                .iter()
                .all(|issue| BATCH_HANDLED.contains(&issue.code)),
            "MPEG source needs unsupported metadata or timing handling"
        );
        let metadata = self
            .assessment
            .metadata
            .as_ref()
            .context("MPEG metadata unavailable")?;
        ensure!(
            metadata.compilation != Some(true) || metadata.album_artist.is_some(),
            "MPEG compilation needs explicit album artist"
        );
        Ok((
            metadata,
            self.assessment
                .coded_timing_proposal
                .as_ref()
                .context("MPEG coded timing unavailable")?,
        ))
    }
    /// The insert-batch profile: any source whose only issues are in
    /// `BATCH_HANDLED`, so MPEG-1 Layer III at any bit rate, with or without
    /// an Info, Xing or VBRI header, or another format, with any projected
    /// tags and any pictures (the one `mpeg::cover_index` chooses is sent with
    /// the audio). Groups may already exist.
    pub(crate) fn ensure_batch_insertion_allowed(&self) -> Result<()> {
        let unhandled: Vec<_> = self
            .assessment
            .batch_refusals()
            .map(|issue| issue.detail.as_str())
            .collect();
        let label = self.source.format().label();
        ensure!(
            unhandled.is_empty(),
            "{label} source is outside the insert-batch profile: {}",
            unhandled.join("; ")
        );
        self.coded_insert_fields().map(|_| ())
    }

    /// What the row of an inserted MP3 must say: its projected tags and the
    /// display duration its insert sent.
    pub(crate) fn expected(
        &self,
    ) -> Result<music_protocol::catalogue::insert_verification::ExpectedSource<'_>> {
        let (m, timing) = self.coded_insert_fields()?;
        Ok(
            music_protocol::catalogue::insert_verification::ExpectedSource::Projected {
                tags: m.scalar(),
                total_time_ms: timing.total_time_ms_floor,
            },
        )
    }

    /// The catalogue fields an insert batch gives this source, in the form a
    /// source-driven Update compares: its projected tags, its sort title and
    /// comment, and the coded duration as the display time. A lossy format
    /// has no bit depth.
    pub(crate) fn projected(&self) -> Result<music_protocol::media::metadata::ProjectedFields> {
        self.ensure_batch_insertion_allowed()?;
        let (m, timing) = self.coded_insert_fields()?;
        Ok(music_protocol::media::metadata::ProjectedFields::from_mpeg(
            m, timing,
        ))
    }

    pub(crate) fn operation_with_metadata(
        &self,
        envelope: music_protocol::protocol::mpeg::Envelope,
        ids: music_protocol::protocol::mpeg::Ids,
        catalog: &music_protocol::catalogue::sqlite::Catalog,
    ) -> Result<Vec<u8>> {
        let (metadata, timing) = self.coded_insert_fields()?;
        let tags = self.expected()?.projected()?;
        let presentation = music_protocol::catalogue::presentation::presentation(
            &tags, catalog, ids.album, ids.artist,
        )?;
        let existing_composer = metadata
            .composer
            .as_deref()
            .map(|name| catalog.composer_for_label(name))
            .transpose()?
            .flatten()
            .map(|row| {
                Ok::<_, anyhow::Error>((
                    row.name.as_str(),
                    row.sort_name
                        .as_deref()
                        .context("existing composer has no qualified sort label")?,
                ))
            })
            .transpose()?;
        music_protocol::protocol::mpeg::serialize_source(
            envelope,
            ids,
            metadata,
            timing,
            &presentation,
            existing_composer,
        )
    }
}

#[cfg(test)]
mod tests;
