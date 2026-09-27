//! The tags and coded timing an insert sends for a non-ALAC source: what a
//! source's inspection projects into the phone's catalogue fields, and the
//! duration, bit rate and gapless fields Apple's rule gives it. This is
//! mechanism only: whether a source may be inserted at all, and how a plan
//! names what it cannot send, stay with the application.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    Source, Tags,
    gapless::Gapless,
    timing::{BitrateMode, SeekHeader},
};
use crate::media::{
    audio_source::Stream,
    format::Format,
    metadata::{ScalarMetadata, tag_values},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Metadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub copyright: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Explicit sort tags; see `ScalarMetadata`'s sort names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_composer: Option<String>,
}
