//! Ordinary source metadata projection shared by planning and offline candidates.
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
