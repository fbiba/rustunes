//! Source-driven metadata Update of a track this tool inserted: the catalogue
//! fields that differ from its source's tags become one explicit edit request,
//! which `sync` sends in verified metadata batches
//! (`execution::metadata_batch::update_all`), in as many sessions as the
//! receiver's group rules need. The encoded audio is the same, so the file on
//! the phone is kept; its container may still carry the old tags.
use anyhow::{Result, bail, ensure};

use super::{FieldDifference, ProjectedFields};
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
