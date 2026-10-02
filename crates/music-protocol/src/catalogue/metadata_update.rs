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

/// Capture one supported available local track, including nonempty observed
/// title/sort and existing album/artist identity. Nullable composer/genre remain
/// exact observed None, rather than invented text or clearing requests.
pub fn bind_source_metadata(catalog: &Catalog, target: u64) -> Result<SourceMetadataBinding> {
    let title = bind_title(catalog, target)?;
    let before = target_track(catalog, target)?.clone();
    ensure!(
        before.artist_sync_id != 0,
        "source metadata artist identity missing"
    );
    Ok(SourceMetadataBinding { title, before })
}

/// Compose exactly one track update and one database-info update. No partial
/// operation escapes when any requested field is unsupported or invalid.
pub fn prepare_source_metadata(
    catalog: &Catalog,
    binding: &SourceMetadataBinding,
    patch: &SourceMetadataPatch,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let accepted = normalize_source_metadata(catalog, binding, patch)?;
    operation(catalog, binding.target(), accepted.item, timestamp)
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

pub(crate) fn normalize_source_metadata(
    catalog: &Catalog,
    binding: &SourceMetadataBinding,
    patch: &SourceMetadataPatch,
) -> Result<NormalizedSourceMetadata> {
    ensure!(
        *binding == bind_source_metadata(catalog, binding.target())?,
        "source metadata binding differs from current catalogue"
    );
    let accepted =
        normalize_source_metadata_item(&binding.before, &binding.before_title().sort_name, patch)?;
    reject_shared_case_alias(catalog, &binding.before, &accepted.scalar)?;
    Ok(accepted)
}

// Descriptive data normalization only. Catalogue rebinding, album membership and
// shared lookup admission stay in normalize_source_metadata, never in this helper.
fn normalize_source_metadata_item(
    before: &Track,
    before_sort: &str,
    patch: &SourceMetadataPatch,
) -> Result<NormalizedSourceMetadata> {
    ensure_available(before)?;
    title_text(&before.metadata.title)?;
    title_text(before_sort)?;
    ensure!(before.album_sync_id != 0, "title album identity missing");
    ensure!(
        before.artist_sync_id != 0,
        "source metadata artist identity missing"
    );
    reject_unsupported_source_fields(patch)?;
    let title = match (
        requested_text(&patch.title, "title")?,
        requested_text(&patch.sort_name, "sort_name")?,
    ) {
        (None, None) => None,
        (title, sort_name) => Some(TitlePatch {
            title: title.unwrap_or(&before.metadata.title).to_owned(),
            sort_name: sort_name.unwrap_or(before_sort).to_owned(),
        }),
    };
    if let Some(title) = &title {
        validate_title_pair(&before.metadata.title, before_sort, title)?;
    }
    let scalar = ScalarPatch {
        composer: requested_text(&patch.composer, "composer")?.map(str::to_owned),
        genre: requested_text(&patch.genre, "genre")?.map(str::to_owned),
        disc_number: requested_number(&patch.disc, "disc")?,
        disc_count: requested_number(&patch.disc_total, "disc_total")?,
        track_number: requested_number(&patch.track, "track")?,
        track_count: requested_number(&patch.track_total, "track_total")?,
        year: requested_number(&patch.year, "year")?,
    };
    let (mut expected, mut item) = if scalar == ScalarPatch::default() {
        (before.clone(), Dictionary::new())
    } else {
        apply(before, &scalar)?
    };
    let mut changed_fields: Vec<String> = item.keys().cloned().collect();
    let mut expected_sort = before_sort.to_owned();
    if let Some(title) = &title {
        append_title_item(&mut item, before.album_sync_id, title);
        if title.title != before.metadata.title {
            changed_fields.push("title".into());
        }
        if title.sort_name != before_sort {
            changed_fields.push("sort_name".into());
        }
        expected.metadata.title = title.title.clone();
        expected_sort = title.sort_name.clone();
    }
    ensure!(!item.is_empty(), "empty source metadata patch");
    Ok(NormalizedSourceMetadata {
        expected,
        expected_sort,
        changed_fields,
        item,
        scalar,
    })
}

mod operation_data;
pub use operation_data::validate_source_metadata_operation_data;

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
pub fn bind_track_number(catalog: &Catalog, target: u64) -> Result<TrackNumberBinding> {
    let t = target_track(catalog, target)?;
    ensure_available(t)?;
    let number = u16::try_from(t.metadata.track_number)?;
    let count = u16::try_from(t.metadata.track_count)?;
    ensure!(
        number > 0 && count > 0 && number <= count,
        "track-number requires positive existing number and total"
    );
    ensure!(
        !t.metadata.is_compilation && t.album_sync_id != 0,
        "track-number first profile requires an ordinary album"
    );
    let mut albums = catalog
        .albums()
        .values()
        .filter(|a| a.sync_id == t.album_sync_id);
    let album = albums.next().context("track-number album absent")?;
    ensure!(
        albums.next().is_none()
            && album.name == t.metadata.album
            && album.live_track_count == 1
            && !album.all_compilations
            && !album.has_compilation_tracks,
        "track-number first profile requires an unambiguous singleton album"
    );
    ensure!(
        !catalog
            .artwork()
            .tracks_with_artwork_registration()
            .contains(&target)
            && !catalog
                .artwork()
                .album_cache_ids()
                .contains_key(&t.album_sync_id),
        "track-number first profile requires no target or album artwork"
    );
    catalog
        .persisted_media_anchor()
        .checked_add(1)
        .context("Media anchor exhausted")?;
    ensure!(
        catalog.database_pid() != 0 && catalog.primary_container_pid() != 0,
        "catalog identity missing"
    );
    Ok(TrackNumberBinding {
        target,
        album: t.album_sync_id,
        number,
        count,
        database: catalog.database_pid(),
        primary: catalog.primary_container_pid(),
        anchor: catalog.persisted_media_anchor(),
        database_sha256: catalog.source_hashes().database,
        wal_sha256: catalog.source_hashes().wal,
    })
}
pub fn prepare_track_number(
    catalog: &Catalog,
    binding: &TrackNumberBinding,
    number: u16,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    ensure!(
        *binding == bind_track_number(catalog, binding.target)?,
        "track-number binding differs from current catalogue"
    );
    ensure!(
        number > 0 && number <= binding.count && number != binding.number,
        "track-number change must be positive, within the unchanged total, and different"
    );
    let mut item = Dictionary::new();
    item.insert("track_number".into(), u64::from(number).into());
    operation(catalog, binding.target, item, timestamp)
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

/// Capture the exact original title and nullable SQL sort-title from the same
/// bounded catalogue read. Empty/absent sort restoration is not qualified.
pub fn bind_title(catalog: &Catalog, target: u64) -> Result<TitleBinding> {
    let track = target_track(catalog, target)?;
    ensure_available(track)?;
    let sort = catalog
        .title_sort(target)
        .context("title-sort track binding absent")?
        .as_deref()
        .context("original title sort is absent; clearing restoration unsupported")?;
    title_text(&track.metadata.title)?;
    title_text(sort)?;
    ensure!(track.album_sync_id != 0, "title album identity missing");
    let mut albums = catalog
        .albums()
        .values()
        .filter(|album| album.sync_id == track.album_sync_id);
    let album = albums.next().context("title album binding absent")?;
    ensure!(
        albums.next().is_none() && album.name == track.metadata.album,
        "title album binding is ambiguous or mismatched"
    );
    ensure!(
        catalog.database_pid() != 0 && catalog.primary_container_pid() != 0,
        "catalog identity missing"
    );
    catalog
        .persisted_media_anchor()
        .checked_add(1)
        .context("Media anchor exhausted")?;
    let hashes = catalog.source_hashes();
    Ok(TitleBinding {
        target,
        album_pid: track.album_sync_id,
        before: TitlePatch {
            title: track.metadata.title.clone(),
            sort_name: sort.to_owned(),
        },
        database_pid: catalog.database_pid(),
        primary_container_pid: catalog.primary_container_pid(),
        anchor: catalog.persisted_media_anchor(),
        database_sha256: hashes.database,
        wal_sha256: hashes.wal,
        database_bytes: hashes.database_bytes,
        wal_bytes: hashes.wal_bytes,
    })
}

/// Serialize the native title2/sort78 selection with its album_pid77 dependency.
/// No operation is sent, source changed, ID allocated or ownership
/// inferred. A restoration must bind a newly observed catalogue independently.
pub fn prepare_title(
    catalog: &Catalog,
    binding: &TitleBinding,
    patch: &TitlePatch,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    validate_title_change(catalog, binding, patch)?;
    let mut item = Dictionary::new();
    append_title_item(&mut item, binding.album_pid, patch);
    operation(catalog, binding.target, item, timestamp)
}

fn append_title_item(item: &mut Dictionary, album_pid: u64, patch: &TitlePatch) {
    item.insert("title".into(), patch.title.clone().into());
    item.insert("sort_name".into(), patch.sort_name.clone().into());
    item.insert("album_pid".into(), (album_pid as i64).into());
}

/// Validate observed/requested state without constructing any operation bytes.
pub(crate) fn validate_title_change(
    catalog: &Catalog,
    binding: &TitleBinding,
    patch: &TitlePatch,
) -> Result<()> {
    ensure!(
        *binding == bind_title(catalog, binding.target)?,
        "title binding differs from current catalogue state or group"
    );
    validate_title_pair(&binding.before.title, &binding.before.sort_name, patch)
}

fn validate_title_pair(before_title: &str, before_sort: &str, patch: &TitlePatch) -> Result<()> {
    title_text(&patch.title)?;
    title_text(&patch.sort_name)?;
    ensure!(
        patch.title != before_title || patch.sort_name != before_sort,
        "title/sort request is already equal"
    );
    Ok(())
}

fn title_text(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
        "invalid explicit title/sort; empty clearing unsupported"
    );
    Ok(())
}

pub fn prepare(
    catalog: &Catalog,
    target: u64,
    patch: &ScalarPatch,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let original = target_track(catalog, target)?;
    let (_, item) = apply(original, patch)?;
    reject_shared_case_alias(catalog, original, patch)?;
    operation(catalog, target, item, timestamp)
}

fn operation(
    catalog: &Catalog,
    target: u64,
    item: Dictionary,
    timestamp: SystemTime,
) -> Result<Vec<u8>> {
    let value = operation_value(
        catalog.database_pid(),
        catalog.primary_container_pid(),
        catalog.persisted_media_anchor(),
        target,
        item,
        timestamp.into(),
    )?;
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes)?;
    Ok(bytes)
}

// Shared canonical envelope. IDs here are descriptive values, not authority.
fn operation_value(
    database_pid: u64,
    primary_container_pid: u64,
    base_anchor: u32,
    target: u64,
    item: Dictionary,
    timestamp: plist::Date,
) -> Result<Value> {
    ensure!(
        database_pid != 0 && primary_container_pid != 0,
        "catalog identity missing"
    );
    let revision = base_anchor
        .checked_add(1)
        .context("Media anchor exhausted")?;
    let mut db_info = Dictionary::new();
    db_info.insert(
        "primary_container_pid".into(),
        (primary_container_pid as i64).into(),
    );
    let mut db = Dictionary::new();
    db.insert("operation".into(), "update_db_info".into());
    db.insert("pid".into(), (database_pid as i64).into());
    db.insert("db_info".into(), Value::Dictionary(db_info));
    let mut update = Dictionary::new();
    update.insert("operation".into(), "update_track".into());
    update.insert("pid".into(), (target as i64).into());
    update.insert("item".into(), Value::Dictionary(item));
    let mut root = Dictionary::new();
    root.insert("revision".into(), revision.into());
    root.insert("timestamp".into(), Value::Date(timestamp));
    root.insert(
        "operations".into(),
        Value::Array(vec![Value::Dictionary(db), Value::Dictionary(update)]),
    );
    Ok(Value::Dictionary(root))
}
/// Resolve exactly one local catalogue track under the serializer's supported shape.
/// Ownership and execution authorization remain caller responsibilities.
pub fn target_track(catalog: &Catalog, target: u64) -> Result<&Track> {
    ensure!(target != 0, "zero metadata target");
    let mut matches = catalog.tracks().iter().filter(|t| t.sync_id == target);
    let track = matches.next().context("metadata target absent")?;
    ensure!(matches.next().is_none(), "ambiguous metadata target");
    Ok(track)
}
pub(crate) fn reject_shared_case_alias(
    catalog: &Catalog,
    target: &Track,
    patch: &ScalarPatch,
) -> Result<()> {
    reject_shared_case_alias_in(catalog.tracks(), target, patch)
}
fn reject_shared_case_alias_in(
    tracks: &[Track],
    target: &Track,
    patch: &ScalarPatch,
) -> Result<()> {
    for (label, old, new) in [
        (
            "composer",
            target.metadata.composer.as_deref(),
            patch.composer.as_deref(),
        ),
        (
            "genre",
            target.metadata.genre.as_deref(),
            patch.genre.as_deref(),
        ),
    ] {
        if let (Some(old), Some(new)) = (old, new)
            && old != new
            && old.eq_ignore_ascii_case(new)
        {
            ensure!(
                !tracks.iter().any(|t| t.sync_id != target.sync_id
                    && match label {
                        "composer" => t.metadata.composer.as_deref() == Some(old),
                        _ => t.metadata.genre.as_deref() == Some(old),
                    }),
                "shared {label} case-only change needs separately qualified lookup-group behavior"
            );
        }
    }
    Ok(())
}
pub(crate) fn apply(track: &Track, patch: &ScalarPatch) -> Result<(Track, Dictionary)> {
    ensure_available(track)?;
    let mut expected = track.clone();
    let mut item = Dictionary::new();
    for (name, request, field) in [
        ("composer", &patch.composer, &mut expected.metadata.composer),
        ("genre", &patch.genre, &mut expected.metadata.genre),
    ] {
        if let Some(value) = request {
            ensure!(
                !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
                "invalid explicit {name}; empty clearing unsupported"
            );
            ensure!(
                field.as_ref() != Some(value),
                "requested {name} is already equal"
            );
            *field = Some(value.clone());
            item.insert(name.into(), value.clone().into());
        }
    }
    for (name, request, field) in [
        (
            "disc_number",
            patch.disc_number,
            &mut expected.metadata.disc_number,
        ),
        (
            "disc_count",
            patch.disc_count,
            &mut expected.metadata.disc_count,
        ),
        (
            "track_number",
            patch.track_number,
            &mut expected.metadata.track_number,
        ),
        (
            "track_count",
            patch.track_count,
            &mut expected.metadata.track_count,
        ),
        ("year", patch.year, &mut expected.metadata.year),
    ] {
        if let Some(value) = request {
            ensure!(
                value > 0,
                "explicit {name} must be positive; zero clearing unsupported"
            );
            ensure!(
                *field != u32::from(value),
                "requested {name} is already equal"
            );
            *field = u32::from(value);
            item.insert(name.into(), u64::from(value).into());
        }
    }
    ensure!(!item.is_empty(), "empty metadata patch");
    for (number, total) in [
        (expected.metadata.disc_number, expected.metadata.disc_count),
        (
            expected.metadata.track_number,
            expected.metadata.track_count,
        ),
    ] {
        ensure!(
            total == 0 || number <= total,
            "requested numbering exceeds known total"
        );
    }
    Ok((expected, item))
}

fn ensure_available(track: &Track) -> Result<()> {
    ensure!(
        track.sync_id != 0
            && track.afc_path.is_some()
            && !track.availability.is_placeholder
            && track.availability.is_library_added
            && track.availability.has_non_purgeable_asset
            && track.availability.is_supported_type
            && track.metadata.file_size > 0
            && track.metadata.total_time_ms > 0,
        "metadata target is not supported available local audio"
    );
    Ok(())
}

#[cfg(test)]
mod composed_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_number_wire_is_exact_single_field_and_frozen_binding() {
        let catalog = crate::test_support::catalogue::collection_catalog(12);
        let binding = bind_track_number(&catalog, u64::MAX).unwrap();
        assert_eq!((binding.before_number(), binding.track_count()), (6, 10));
        let bytes = prepare_track_number(&catalog, &binding, 7, SystemTime::UNIX_EPOCH).unwrap();
        let value = Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(root["revision"].as_unsigned_integer(), Some(13));
        let ops = root["operations"].as_array().unwrap();
        assert_eq!(ops.len(), 2);
        let track = ops[1].as_dictionary().unwrap();
        assert_eq!(track["pid"].as_signed_integer(), Some(-1));
        let item = track["item"].as_dictionary().unwrap();
        assert_eq!(item.len(), 1);
        assert_eq!(item["track_number"].as_unsigned_integer(), Some(7));
        for number in [0, 6, 11, u16::MAX] {
            assert!(
                prepare_track_number(&catalog, &binding, number, SystemTime::UNIX_EPOCH).is_err()
            );
        }
        for sql in [
            "UPDATE item SET track_number=0",
            "UPDATE item_extra SET track_count=0",
            "UPDATE item_extra SET track_count=65536",
            "UPDATE item SET track_number=11",
            "UPDATE item_extra SET track_count=9",
        ] {
            let changed = crate::test_support::catalogue::modified_collection_catalog(12, sql);
            assert!(
                prepare_track_number(&changed, &binding, 7, SystemTime::UNIX_EPOCH).is_err(),
                "{sql}"
            );
        }
        let max = crate::test_support::catalogue::modified_collection_catalog(
            12,
            "UPDATE item_extra SET track_count=65535",
        );
        let binding = bind_track_number(&max, u64::MAX).unwrap();
        assert!(prepare_track_number(&max, &binding, u16::MAX, SystemTime::UNIX_EPOCH).is_ok());
    }

    fn title_patch() -> TitlePatch {
        TitlePatch {
            title: "Τίτλος Α".into(),
            sort_name: "Title A".into(),
        }
    }

    #[test]
    fn title_operation_has_exact_native_three_field_item_and_unchanged_db_info() {
        let catalog = crate::test_support::catalogue::collection_catalog(12);
        let binding = bind_title(&catalog, u64::MAX).unwrap();
        assert_eq!(binding.target(), u64::MAX);
        assert_eq!(binding.album_pid(), (-31_i64) as u64);
        assert_eq!(binding.before().title, "Synthetic Title");
        assert_eq!(binding.before().sort_name, "Synthetic Title");
        let bytes =
            prepare_title(&catalog, &binding, &title_patch(), SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(
            bytes,
            prepare_title(&catalog, &binding, &title_patch(), SystemTime::UNIX_EPOCH).unwrap()
        );
        let actual = Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
        let expected = Value::from_reader_xml(std::io::Cursor::new(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict>
              <key>revision</key><integer>13</integer>
              <key>timestamp</key><date>1970-01-01T00:00:00Z</date>
              <key>operations</key><array>
                <dict><key>operation</key><string>update_db_info</string>
                  <key>pid</key><integer>-10</integer>
                  <key>db_info</key><dict><key>primary_container_pid</key><integer>-11</integer></dict>
                </dict>
                <dict><key>operation</key><string>update_track</string>
                  <key>pid</key><integer>-1</integer>
                  <key>item</key><dict>
                    <key>title</key><string>Τίτλος Α</string>
                    <key>sort_name</key><string>Title A</string>
                    <key>album_pid</key><integer>-31</integer>
                  </dict>
                </dict>
              </array>
            </dict></plist>"#,
        ))
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn title_pid_bits_use_signed_plist_integers_at_all_boundaries() {
        let cases = [1_u64, i64::MAX as u64, 1_u64 << 63, u64::MAX];
        for i in 0..cases.len() {
            let [target, album, database, primary] =
                std::array::from_fn(|n| cases[(i + n) % cases.len()] as i64);
            let catalog = crate::test_support::catalogue::modified_collection_catalog(
                12,
                &format!(
                    "UPDATE item_store SET sync_id={target};
                     UPDATE album SET sync_id={album} WHERE album_pid=31;
                     UPDATE db_info SET db_pid={database},primary_container_pid={primary};
                     UPDATE container SET sync_id={primary};"
                ),
            );
            let binding = bind_title(&catalog, target as u64).unwrap();
            let value = Value::from_reader(std::io::Cursor::new(
                prepare_title(&catalog, &binding, &title_patch(), SystemTime::UNIX_EPOCH).unwrap(),
            ))
            .unwrap();
            let ops = value.as_dictionary().unwrap()["operations"]
                .as_array()
                .unwrap();
            let db = ops[0].as_dictionary().unwrap();
            let own = ops[1].as_dictionary().unwrap();
            assert_eq!(db["pid"].as_signed_integer(), Some(database));
            assert_eq!(
                db["db_info"].as_dictionary().unwrap()["primary_container_pid"].as_signed_integer(),
                Some(primary)
            );
            assert_eq!(own["pid"].as_signed_integer(), Some(target));
            assert_eq!(
                own["item"].as_dictionary().unwrap()["album_pid"].as_signed_integer(),
                Some(album)
            );
        }
    }

    #[test]
    fn title_binding_refuses_absence_unavailable_target_or_unrestorable_sort() {
        let catalog = crate::test_support::catalogue::collection_catalog(12);
        for target in [0, 123] {
            assert!(bind_title(&catalog, target).is_err());
        }
        for sql in [
            "UPDATE item_extra SET sort_title=NULL",
            "UPDATE item_extra SET sort_title=''",
            "UPDATE item_extra SET title=''",
            "UPDATE item_state SET is_placeholder=1",
            "UPDATE item_state SET has_non_purgeable_asset=0",
        ] {
            let changed = crate::test_support::catalogue::modified_collection_catalog(12, sql);
            assert!(bind_title(&changed, u64::MAX).is_err(), "{sql}");
        }
        let exhausted = crate::test_support::catalogue::collection_catalog(u32::MAX);
        assert!(bind_title(&exhausted, u64::MAX).is_err());
    }

    #[test]
    fn title_binding_rejects_group_identity_anchor_hash_and_before_value_mismatch() {
        let catalog = crate::test_support::catalogue::collection_catalog(12);
        let binding = bind_title(&catalog, u64::MAX).unwrap();
        for variant in 0..12 {
            let mut wrong = binding.clone();
            match variant {
                0 => wrong.album_pid += 1,
                1 => wrong.database_pid += 1,
                2 => wrong.primary_container_pid += 1,
                3 => wrong.anchor += 1,
                4 => wrong.database_sha256[0] ^= 1,
                5 => wrong.wal_sha256 = Some([1; 32]),
                6 => wrong.database_bytes += 1,
                7 => wrong.wal_bytes = Some(123),
                8 => wrong.before.title.push('!'),
                9 => wrong.before.sort_name.push('!'),
                10 => wrong.target = 0,
                _ => wrong.target = 123,
            }
            assert!(
                prepare_title(&catalog, &wrong, &title_patch(), SystemTime::UNIX_EPOCH).is_err()
            );
        }
        // Even an unrelated SQL change makes the old capture binding stale.
        let changed = crate::test_support::catalogue::modified_collection_catalog(
            12,
            "UPDATE item_stats SET user_rating=20",
        );
        assert!(prepare_title(&changed, &binding, &title_patch(), SystemTime::UNIX_EPOCH).is_err());
    }

    #[test]
    fn title_patch_rejects_noop_empty_control_or_overlong_values() {
        let catalog = crate::test_support::catalogue::collection_catalog(12);
        let binding = bind_title(&catalog, u64::MAX).unwrap();
        assert!(
            prepare_title(&catalog, &binding, binding.before(), SystemTime::UNIX_EPOCH).is_err()
        );
        for invalid in [
            String::new(),
            " \t".into(),
            "bad\0text".into(),
            "x".repeat(4097),
        ] {
            for sort in [false, true] {
                let mut patch = title_patch();
                if sort {
                    patch.sort_name = invalid.clone();
                } else {
                    patch.title = invalid.clone();
                }
                assert!(prepare_title(&catalog, &binding, &patch, SystemTime::UNIX_EPOCH).is_err());
            }
        }
        let mut sort_only = binding.before().clone();
        sort_only.sort_name = "Explicit different sort".into();
        assert!(prepare_title(&catalog, &binding, &sort_only, SystemTime::UNIX_EPOCH).is_ok());
    }

    #[test]
    fn title_restoration_requires_new_observed_binding_and_next_revision() {
        let before = crate::test_support::catalogue::collection_catalog(12);
        let first = bind_title(&before, u64::MAX).unwrap();
        let after = crate::test_support::catalogue::modified_collection_catalog(
            13,
            "UPDATE item_extra SET title='Changed title',sort_title='Changed sort'",
        );
        assert!(prepare_title(&after, &first, first.before(), SystemTime::UNIX_EPOCH).is_err());
        let second = bind_title(&after, u64::MAX).unwrap();
        let restored = Value::from_reader(std::io::Cursor::new(
            prepare_title(&after, &second, first.before(), SystemTime::UNIX_EPOCH).unwrap(),
        ))
        .unwrap();
        let root = restored.as_dictionary().unwrap();
        assert_eq!(root["revision"].as_unsigned_integer(), Some(14));
        let own = root["operations"].as_array().unwrap()[1]
            .as_dictionary()
            .unwrap();
        let item = own["item"].as_dictionary().unwrap();
        assert_eq!(item["title"].as_string(), Some("Synthetic Title"));
        assert_eq!(item["sort_name"].as_string(), Some("Synthetic Title"));
        assert_eq!(item["album_pid"].as_signed_integer(), Some(-31));
    }

    #[test]
    fn rejects_observed_shared_case_only_lookup_change() {
        let c = crate::test_support::catalogue::collection_catalog(12);
        let target = &c.tracks()[0];
        let patch = ScalarPatch {
            composer: Some("SYNTHETIC COMPOSER".into()),
            ..Default::default()
        };
        assert!(reject_shared_case_alias_in(c.tracks(), target, &patch).is_ok());
        let mut tracks = c.tracks().to_vec();
        let mut sibling = target.clone();
        sibling.sync_id = 123;
        tracks.push(sibling);
        assert!(reject_shared_case_alias_in(&tracks, target, &patch).is_err());
        let other = ScalarPatch {
            composer: Some("Unrelated explicit composer".into()),
            ..Default::default()
        };
        assert!(reject_shared_case_alias_in(&tracks, target, &other).is_ok());
    }
    #[test]
    fn positive_scalar_fields_have_exact_numeric_and_text_wire_types() {
        let c = crate::test_support::catalogue::collection_catalog(12);
        let p = ScalarPatch {
            composer: Some("Explicit composer".into()),
            genre: Some("Explicit genre".into()),
            disc_number: Some(2),
            disc_count: Some(3),
            track_number: Some(7),
            track_count: Some(12),
            year: Some(2025),
        };
        let (track, item) = apply(&c.tracks()[0], &p).unwrap();
        assert_eq!(item.len(), 7);
        for (name, value) in [
            ("disc_number", 2),
            ("disc_count", 3),
            ("track_number", 7),
            ("track_count", 12),
            ("year", 2025),
        ] {
            assert_eq!(item[name].as_unsigned_integer(), Some(value));
        }
        assert_eq!(item["genre"].as_string(), Some("Explicit genre"));
        assert_eq!(item["composer"].as_string(), Some("Explicit composer"));
        assert_eq!(track.playback, c.tracks()[0].playback);
        assert_eq!(track.afc_path, c.tracks()[0].afc_path);
    }
    #[test]
    fn emits_only_requested_scalar_keys_and_current_identities() {
        let c = crate::test_support::catalogue::collection_catalog(12);
        let patch = ScalarPatch {
            composer: Some("Literal; Composer".into()),
            disc_number: Some(2),
            ..Default::default()
        };
        let bytes = prepare(&c, u64::MAX, &patch, SystemTime::UNIX_EPOCH).unwrap();
        let value = Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
        let root = value.as_dictionary().unwrap();
        assert_eq!(root.len(), 3);
        assert_eq!(root["revision"].as_unsigned_integer(), Some(13));
        let ops = root["operations"].as_array().unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0].as_dictionary().unwrap()["operation"].as_string(),
            Some("update_db_info")
        );
        let own = ops[1].as_dictionary().unwrap();
        assert_eq!(own.len(), 3);
        assert_eq!(own["pid"].as_signed_integer(), Some(-1));
        assert_eq!(own["operation"].as_string(), Some("update_track"));
        let item = own["item"].as_dictionary().unwrap();
        assert_eq!(item.len(), 2);
        assert_eq!(item["composer"].as_string(), Some("Literal; Composer"));
        assert_eq!(item["disc_number"].as_unsigned_integer(), Some(2));
    }
    #[test]
    fn refuses_absence_clearing_noop_invalid_text_and_numbering() {
        let c = crate::test_support::catalogue::collection_catalog(12);
        for p in [
            ScalarPatch::default(),
            ScalarPatch {
                disc_number: Some(0),
                ..Default::default()
            },
            ScalarPatch {
                composer: Some(String::new()),
                ..Default::default()
            },
            ScalarPatch {
                composer: Some("bad\0text".into()),
                ..Default::default()
            },
            ScalarPatch {
                disc_number: Some(1),
                ..Default::default()
            },
            ScalarPatch {
                track_number: Some(11),
                ..Default::default()
            },
        ] {
            assert!(prepare(&c, u64::MAX, &p, SystemTime::UNIX_EPOCH).is_err());
        }
        assert!(
            prepare(
                &c,
                55,
                &ScalarPatch {
                    year: Some(2025),
                    ..Default::default()
                },
                SystemTime::UNIX_EPOCH
            )
            .is_err()
        );
        assert!(
            prepare(
                &crate::test_support::catalogue::collection_catalog(u32::MAX),
                u64::MAX,
                &ScalarPatch {
                    year: Some(2025),
                    ..Default::default()
                },
                SystemTime::UNIX_EPOCH
            )
            .is_err()
        );
    }
}
