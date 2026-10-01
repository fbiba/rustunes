//! Pure, explicit scalar updates for an existing local Music track.
//! No source absence becomes a zero/empty clearing operation. The shared pinned
//! native item writer establishes these field names/types; receiver behavior
//! still needs a controlled metadata-only qualification. No ownership is inferred.
use crate::catalogue::sqlite::{Catalog, Track};
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// None always preserves. Text must be nonempty; numbers must be positive.
/// Album/artist identities, sort keys, audio, artwork and statistics are outside
/// this type. Source parsing/projection and selecting a requested change belong
/// to the caller; passing a patch is not synchronization/deletion authorization.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScalarPatch {
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub disc_number: Option<u16>,
    pub disc_count: Option<u16>,
    pub track_number: Option<u16>,
    pub track_count: Option<u16>,
    pub year: Option<u16>,
}

/// An explicit title/sort pair, separate from ordinary scalar updates. This
/// does not change source-file tags or authorize a device/journal operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TitlePatch {
    pub title: String,
    pub sort_name: String,
}

/// Explicit source intent. Preserve never means absence; Clear is a requested
/// mutation and currently refuses native serialization. Set cannot contain JSON null.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SourceFieldRequest<T> {
    #[default]
    Preserve,
    Set(T),
    Clear,
}

/// Every field of the source projection except `comment` (21 fields) is
/// represented explicitly.
/// Deserialization requires every field and rejects duplicates/unknown keys.
/// Rust Default is the explicit preserve-all request, not source-tag inference.
/// The supported subset is title/sort plus existing positive scalar encodings;
/// requesting any other field blocks the entire operation. No receiver, SQL,
/// ownership, source inspection or application authority is established here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMetadataPatch {
    pub title: SourceFieldRequest<String>,
    pub sort_name: SourceFieldRequest<String>,
    pub artist: SourceFieldRequest<String>,
    pub album: SourceFieldRequest<String>,
    pub album_artist: SourceFieldRequest<String>,
    pub grouping_artist: SourceFieldRequest<String>,
    pub genre: SourceFieldRequest<String>,
    pub composer: SourceFieldRequest<String>,
    pub copyright: SourceFieldRequest<String>,
    pub year: SourceFieldRequest<u32>,
    pub content_rating: SourceFieldRequest<u8>,
    pub compilation: SourceFieldRequest<bool>,
    pub track: SourceFieldRequest<u32>,
    pub track_total: SourceFieldRequest<u32>,
    pub disc: SourceFieldRequest<u32>,
    pub disc_total: SourceFieldRequest<u32>,
    pub sample_rate: SourceFieldRequest<u32>,
    pub bit_depth: SourceFieldRequest<u8>,
    pub channels: SourceFieldRequest<u8>,
    pub duration_ticks: SourceFieldRequest<u64>,
    pub total_time_ms: SourceFieldRequest<u64>,
}

impl SourceMetadataPatch {
    /// Pure comparison of caller-admitted projections. This does not admit
    /// either projection or claim that catalogue values match the before one.
    /// Every source field except `comment` participates; removing an optional
    /// tag requests Clear.
    pub fn from_projections(
        before: &crate::media::metadata::ProjectedFields,
        desired: &crate::media::metadata::ProjectedFields,
    ) -> Self {
        Self {
            title: source_change(&before.title, &desired.title),
            sort_name: source_change(&before.sort_name, &desired.sort_name),
            artist: source_change(&before.artist, &desired.artist),
            album: source_change(&before.album, &desired.album),
            album_artist: optional_source_change(&before.album_artist, &desired.album_artist),
            grouping_artist: source_change(&before.grouping_artist, &desired.grouping_artist),
            genre: optional_source_change(&before.genre, &desired.genre),
            composer: optional_source_change(&before.composer, &desired.composer),
            copyright: optional_source_change(&before.copyright, &desired.copyright),
            year: optional_source_change(&before.year, &desired.year),
            content_rating: optional_source_change(&before.content_rating, &desired.content_rating),
            compilation: optional_source_change(&before.compilation, &desired.compilation),
            track: optional_source_change(&before.track, &desired.track),
            track_total: optional_source_change(&before.track_total, &desired.track_total),
            disc: optional_source_change(&before.disc, &desired.disc),
            disc_total: optional_source_change(&before.disc_total, &desired.disc_total),
            sample_rate: source_change(&before.sample_rate, &desired.sample_rate),
            bit_depth: source_change(&before.bit_depth, &desired.bit_depth),
            channels: source_change(&before.channels, &desired.channels),
            duration_ticks: source_change(&before.duration_ticks, &desired.duration_ticks),
            total_time_ms: source_change(&before.total_time_ms, &desired.total_time_ms),
        }
    }
}

fn source_change<T: Clone + PartialEq>(before: &T, desired: &T) -> SourceFieldRequest<T> {
    if before == desired {
        SourceFieldRequest::Preserve
    } else {
        SourceFieldRequest::Set(desired.clone())
    }
}

fn optional_source_change<T: Clone + PartialEq>(
    before: &Option<T>,
    desired: &Option<T>,
) -> SourceFieldRequest<T> {
    if before == desired {
        SourceFieldRequest::Preserve
    } else {
        match desired {
            Some(value) => SourceFieldRequest::Set(value.clone()),
            None => SourceFieldRequest::Clear,
        }
    }
}

/// Private observed fields prevent caller-supplied before values from becoming
/// catalogue facts. Rebinding checks the entire target, nullable scalar values,
/// title/sort, groups and original catalogue hashes/identities/anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMetadataBinding {
    title: TitleBinding,
    before: Track,
}

impl SourceMetadataBinding {
    pub fn target(&self) -> u64 {
        self.title.target()
    }
    pub fn before(&self) -> &Track {
        &self.before
    }
    pub fn before_title(&self) -> &TitlePatch {
        self.title.before()
    }
}

/// One accepted request determines both the wire item and the exact projected
/// poststate. The serializer and verifier must not independently reinterpret
/// Preserve, Set, Clear or the title pair's album dependency.
pub(crate) struct NormalizedSourceMetadata {
    pub(crate) expected: Track,
    pub(crate) expected_sort: String,
    pub(crate) changed_fields: Vec<String>,
    item: Dictionary,
    scalar: ScalarPatch,
}

mod operation_data;

fn requested_text<'a>(
    request: &'a SourceFieldRequest<String>,
    field: &str,
) -> Result<Option<&'a str>> {
    match request {
        SourceFieldRequest::Preserve => Ok(None),
        SourceFieldRequest::Set(value) => Ok(Some(value.as_str())),
        SourceFieldRequest::Clear => {
            anyhow::bail!("source metadata field {field}: metadata_clearing_not_supported")
        }
    }
}

fn requested_number(request: &SourceFieldRequest<u32>, field: &str) -> Result<Option<u16>> {
    match request {
        SourceFieldRequest::Preserve => Ok(None),
        SourceFieldRequest::Clear | SourceFieldRequest::Set(0) => {
            anyhow::bail!("source metadata field {field}: metadata_clearing_not_supported")
        }
        SourceFieldRequest::Set(value) => Ok(Some(u16::try_from(*value).map_err(|_| {
            anyhow::anyhow!("source metadata field {field}: scalar_value_exceeds_u16_encoding")
        })?)),
    }
}

fn reject_source_field<T>(
    request: &SourceFieldRequest<T>,
    field: &str,
    reason: &str,
) -> Result<()> {
    match request {
        SourceFieldRequest::Preserve => Ok(()),
        SourceFieldRequest::Clear => {
            anyhow::bail!("source metadata field {field}: metadata_clearing_not_supported")
        }
        SourceFieldRequest::Set(_) => anyhow::bail!("source metadata field {field}: {reason}"),
    }
}

fn reject_unsupported_source_fields(patch: &SourceMetadataPatch) -> Result<()> {
    for (field, request) in [
        ("artist", &patch.artist),
        ("album", &patch.album),
        ("album_artist", &patch.album_artist),
        ("grouping_artist", &patch.grouping_artist),
    ] {
        reject_source_field(
            request,
            field,
            "group_identity_or_source_grouping_update_unqualified",
        )?;
    }
    reject_source_field(
        &patch.copyright,
        "copyright",
        "copyright_update_encoding_unqualified",
    )?;
    reject_source_field(
        &patch.content_rating,
        "content_rating",
        "content_rating_mapping_and_update_unqualified",
    )?;
    reject_source_field(
        &patch.compilation,
        "compilation",
        "compilation_grouping_update_unqualified",
    )?;
    reject_source_field(
        &patch.sample_rate,
        "sample_rate",
        "audio_attribute_update_requires_replacement",
    )?;
    reject_source_field(
        &patch.bit_depth,
        "bit_depth",
        "audio_attribute_update_requires_replacement",
    )?;
    reject_source_field(
        &patch.channels,
        "channels",
        "audio_attribute_update_requires_replacement",
    )?;
    reject_source_field(
        &patch.duration_ticks,
        "duration_ticks",
        "audio_attribute_update_requires_replacement",
    )?;
    reject_source_field(
        &patch.total_time_ms,
        "total_time_ms",
        "audio_attribute_update_requires_replacement",
    )?;
    Ok(())
}

/// Exact observed numbering. This is separate from title/sort: no numeric
/// value is encoded in text and no album dependency is added to the wire item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackNumberBinding {
    target: u64,
    album: u64,
    number: u16,
    count: u16,
    database: u64,
    primary: u64,
    anchor: u32,
    database_sha256: [u8; 32],
    wal_sha256: Option<[u8; 32]>,
}
impl TrackNumberBinding {
    pub fn album_pid(&self) -> u64 {
        self.album
    }
    pub fn before_number(&self) -> u16 {
        self.number
    }
    pub fn track_count(&self) -> u16 {
        self.count
    }
}

/// Exact parsed catalogue state for one title change. Private fields prevent
/// caller-supplied sort labels from masquerading as observed SQL. This binds
/// catalogue identities/bytes, not a device connection or journal ownership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleBinding {
    target: u64,
    album_pid: u64,
    before: TitlePatch,
    database_pid: u64,
    primary_container_pid: u64,
    anchor: u32,
    database_sha256: [u8; 32],
    wal_sha256: Option<[u8; 32]>,
    database_bytes: u64,
    wal_bytes: Option<u64>,
}

impl TitleBinding {
    pub fn target(&self) -> u64 {
        self.target
    }
    pub fn album_pid(&self) -> u64 {
        self.album_pid
    }
    pub fn before(&self) -> &TitlePatch {
        &self.before
    }
}

fn append_title_item(item: &mut Dictionary, album_pid: u64, patch: &TitlePatch) {
    item.insert("title".into(), patch.title.clone().into());
    item.insert("sort_name".into(), patch.sort_name.clone().into());
    item.insert("album_pid".into(), (album_pid as i64).into());
}
