//! Source-driven metadata Update of a track this tool inserted: the catalogue
//! fields that differ from its source's tags become one explicit edit request,
//! which `sync` sends in verified metadata batches
//! (`execution::metadata_batch::update_all`), in as many sessions as the
//! receiver's group rules need. The encoded audio is the same, so the file on
//! the phone is kept; its container may still carry the old tags.
use anyhow::{Result, bail, ensure};

use super::{FieldDifference, ProjectedFields};
use crate::engine::planning::metadata_edit::EditRequest;
use music_protocol::catalogue::sqlite::{Catalog, Track};

/// A source field only a replacement brings to the phone: a genre or composer
/// the source no longer has. The receiver files a track under either by name,
/// and an empty name only changes the item's sort keys; neither group's id is
/// an item key it reads (docs/05-receiver-operations.md, section 2, "Group
/// fields"). So no edit takes the track out of its group. Removing the track
/// and adding the source again does (`sync --replace-changed`). A cleared
/// number or copyright is an edit (section 2, "Clearing fields").
#[derive(Debug)]
pub(crate) struct NeedsReplacement(pub(crate) &'static str);

impl std::fmt::Display for NeedsReplacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the source has no {}, and an edit cannot clear one on the phone",
            self.0
        )
    }
}

impl std::error::Error for NeedsReplacement {}

/// The edit that brings `track`'s catalogue fields to `desired`, or `None`
/// when they already agree. Names of groups that differ only in ASCII case
/// agree (the phone files them under one group). A number, copyright or
/// comment the source lacks is sent as 0 or `""`, which clears it. Refuses a
/// difference no metadata edit can make: the stored content rating level, the
/// duration, or clearing a genre or composer (`NeedsReplacement`, when nothing
/// else refuses).
pub(crate) fn request(
    desired: &ProjectedFields,
    catalog: &Catalog,
    track: &Track,
) -> Result<Option<EditRequest>> {
    from_differences(
        desired,
        track,
        &super::catalogue_differences(desired, catalog, track),
    )
}

pub(super) fn from_differences(
    desired: &ProjectedFields,
    track: &Track,
    differences: &[FieldDifference],
) -> Result<Option<EditRequest>> {
    if differences.is_empty() {
        return Ok(None);
    }
    let mut request = EditRequest {
        track: track.sync_id,
        ..Default::default()
    };
    // The first field only a replacement clears; a difference no replacement
    // makes either (the duration, say) refuses the whole update first.
    let mut cleared = None::<&'static str>;
    let mut group = |field: &'static str, value: &Option<String>| -> Option<String> {
        match value.as_deref() {
            Some(value) if !value.is_empty() => Some(value.to_owned()),
            _ => {
                cleared = cleared.or(Some(field));
                None
            }
        }
    };
    let number = |field: &'static str, value: Option<u32>| -> Result<Option<u16>> {
        let n = value.unwrap_or(0);
        u16::try_from(n)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("the source's {field} {n} is too large"))
    };
    for difference in differences {
        match difference.field {
            "title" | "sort_name" => {
                request.title = Some(desired.title.clone());
                request.sort_title = Some(desired.sort_name.clone());
            }
            "artist" => request.artist = Some(desired.artist.clone()),
            "album" => request.album = Some(desired.album.clone()),
            "album_artist" => request.album_artist = Some(desired.grouping_artist.clone()),
            "genre" => request.genre = group("genre", &desired.genre),
            "composer" => request.composer = group("composer", &desired.composer),
            "copyright" => request.copyright = Some(desired.copyright.clone().unwrap_or_default()),
            "comment" => request.comment = Some(desired.comment.clone().unwrap_or_default()),
            "year" => request.year = number("year", desired.year)?,
            "track" => request.track_number = number("track number", desired.track)?,
            "track_total" => request.track_count = number("track count", desired.track_total)?,
            // The phone stores disc 1 when an insert names none.
            "disc" => {
                let disc = desired.disc.unwrap_or(1);
                ensure!(disc > 0, "the source's disc number 0 has no phone value");
                request.disc_number = number("disc number", Some(disc))?;
            }
            "disc_total" => request.disc_count = number("disc count", desired.disc_total)?,
            "content_rating" => {
                let rating = desired.content_rating.unwrap_or(0);
                ensure!(
                    rating <= 2,
                    "the source's content rating {rating} has no phone value"
                );
                request.content_rating = Some(rating);
            }
            "compilation" => request.is_compilation = Some(desired.compilation.unwrap_or(false)),
            "content_rating_level" => {
                bail!("the phone's content rating level cannot be set from source tags")
            }
            "total_time_ms" => {
                bail!("the duration differs from the source's; a metadata edit cannot change it")
            }
            other => bail!("{other} cannot be updated"),
        }
    }
    if let Some(field) = cleared {
        return Err(NeedsReplacement(field).into());
    }
    Ok(Some(request))
}

/// The fields a request sends.
#[cfg(test)]
pub(crate) fn fields(request: &EditRequest) -> Vec<&'static str> {
    let mut fields = Vec::new();
    for (field, present) in [
        ("title", request.title.is_some()),
        ("artist", request.artist.is_some()),
        ("album", request.album.is_some()),
        ("album_artist", request.album_artist.is_some()),
        ("genre", request.genre.is_some()),
        ("composer", request.composer.is_some()),
        ("copyright", request.copyright.is_some()),
        ("comment", request.comment.is_some()),
        ("year", request.year.is_some()),
        ("track", request.track_number.is_some()),
        ("track_total", request.track_count.is_some()),
        ("disc", request.disc_number.is_some()),
        ("disc_total", request.disc_count.is_some()),
        ("content_rating", request.content_rating.is_some()),
        ("compilation", request.is_compilation.is_some()),
    ] {
        if present {
            fields.push(field);
        }
    }
    fields
}
