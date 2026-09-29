//! Ordinary source metadata projection shared by planning and offline candidates.
use crate::media::{
    metadata::Metadata,
    mpeg::projection::{CodedTimingProposal, Metadata as MpegMetadata},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedFields {
    pub title: String,
    /// The title's sort name: its sort tag, or the title without a leading
    /// article (`ScalarMetadata::title_sort`).
    pub sort_name: String,
    /// Omitted when the source has no comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub grouping_artist: String,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub copyright: Option<String>,
    pub year: Option<u32>,
    pub content_rating: Option<u8>,
    pub compilation: Option<bool>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u8,
    pub duration_ticks: u64,
    pub total_time_ms: u64,
}

impl ProjectedFields {
    /// The catalogue fields an insert batch gives a non-ALAC source, in the
    /// form a source-driven Update compares: its projected tags, its sort
    /// title and comment, and the coded duration as the display time. A
    /// lossy format has no bit depth.
    pub fn from_mpeg(m: &MpegMetadata, timing: &CodedTimingProposal) -> Self {
        let tags = m.scalar();
        Self {
            title: m.title.clone(),
            sort_name: tags.title_sort().to_owned(),
            comment: m.comment.clone(),
            artist: m.artist.clone(),
            album: m.album.clone(),
            album_artist: m.album_artist.clone(),
            grouping_artist: m.album_artist.clone().unwrap_or_else(|| m.artist.clone()),
            genre: m.genre.clone(),
            composer: m.composer.clone(),
            copyright: m.copyright.clone(),
            year: m.year,
            content_rating: None,
            compilation: m.compilation,
            track: m.track,
            track_total: m.track_total,
            disc: m.disc,
            disc_total: m.disc_total,
            sample_rate: timing.sample_rate,
            bit_depth: timing.bits_per_sample,
            channels: timing.channels,
            duration_ticks: timing.coded_samples,
            total_time_ms: timing.total_time_ms_floor,
        }
    }

    pub fn from_source(source: &Metadata) -> Result<Self> {
        let s = source.projected()?;
        Ok(Self {
            title: s.title.into(),
            sort_name: s.title_sort().into(),
            comment: s.comment.map(str::to_owned),
            artist: s.artist.into(),
            album: s.album.into(),
            album_artist: s.album_artist.map(str::to_owned),
            grouping_artist: s.effective_artist().into(),
            genre: s.genre.map(str::to_owned),
            composer: s.composer.map(str::to_owned),
            copyright: s.copyright.map(str::to_owned),
            year: s.year,
            content_rating: s.content_rating,
            compilation: s.compilation,
            track: s.track,
            track_total: s.track_total,
            disc: s.disc,
            disc_total: s.disc_total,
            sample_rate: source.sample_rate,
            bit_depth: source.bit_depth,
            channels: source.channels,
            duration_ticks: source.timing.duration_ticks,
            total_time_ms: source
                .timing
                .duration_ticks
                .checked_mul(1000)
                .and_then(|n| n.checked_div(u64::from(source.timing.timescale)))
                .context("source duration overflow or zero timescale")?,
        })
    }
}
