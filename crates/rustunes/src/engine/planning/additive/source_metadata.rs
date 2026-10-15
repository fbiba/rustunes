//! Catalogue field values compared against a source's projected tags.
use super::{FieldDifference, ProjectedFields};
use music_protocol::catalogue::sqlite::{Catalog, Track};
use serde_json::{Value, json};

/// Full current values remain distinct from the historic source projection:
/// nullable sort and rating level are observed, never invented from source tags.
pub(super) fn current_fields(catalog: &Catalog, track: &Track) -> Value {
    let m = &track.metadata;
    json!({
        "track_id":format!("{:016x}",track.sync_id),
        "album_id":format!("{:016x}",track.album_sync_id),
        "artist_id":format!("{:016x}",track.artist_sync_id),
        "path":track.afc_path,"file_bytes":m.file_size,
        "title":m.title,"sort_name":catalog.title_sort(track.sync_id),
        "artist":m.artist,"album":m.album,"grouping_artist":m.album_artist,
        "genre":m.genre,"composer":m.composer,"copyright":m.copyright,
        "year":m.year,"content_rating":m.content_rating,
        "content_rating_level":m.content_rating_level,"compilation":m.is_compilation,
        "track":m.track_number,"track_total":m.track_count,
        "disc":m.disc_number,"disc_total":m.disc_count,
        "total_time_ms":m.total_time_ms,
        "artwork_observations":{
            "track_cache_id":catalog.artwork().track_cache_ids().get(&track.sync_id),
            "album_cache_id":catalog.artwork().album_cache_ids().get(&track.album_sync_id),
            "track_registration_present":catalog.artwork().tracks_with_artwork_registration().contains(&track.sync_id)
        },
        "preserved_playback_statistics":{
            "play_count_user":track.playback.play_count_user,
            "play_count_recent":track.playback.play_count_recent,
            "skip_count_user":track.playback.skip_count_user,
            "skip_count_recent":track.playback.skip_count_recent,
            "has_been_played":track.playback.has_been_played,
            "user_rating":track.playback.user_rating,
            "user_rating_is_derived":track.playback.user_rating_is_derived,
            "liked_state":track.playback.liked_state
        }
    })
}

/// Add factual comparisons omitted by the old insertion-oriented matcher.
/// Rating compares raw codes only; their update encoding remains unqualified.
/// A cleared comment is stored as `''`, which is no comment as NULL is.
pub(super) fn additional_differences(
    desired: &ProjectedFields,
    catalog: &Catalog,
    track: &Track,
) -> Vec<FieldDifference> {
    let mut result = Vec::new();
    for (field, current, wanted) in [
        (
            "sort_name",
            json!(catalog.title_sort(track.sync_id)),
            json!(desired.sort_name),
        ),
        (
            "content_rating",
            json!(track.metadata.content_rating),
            json!(desired.content_rating.unwrap_or(0)),
        ),
        (
            "content_rating_level",
            json!(track.metadata.content_rating_level),
            json!(0),
        ),
        (
            "compilation",
            json!(track.metadata.is_compilation),
            json!(desired.compilation.unwrap_or(false)),
        ),
        (
            "comment",
            json!(
                catalog
                    .comment(track.sync_id)
                    .and_then(Option::as_deref)
                    .filter(|text| !text.is_empty())
            ),
            json!(desired.comment),
        ),
    ] {
        if current != wanted {
            result.push(FieldDifference {
                field,
                current,
                desired: wanted,
            });
        }
    }
    result
}
