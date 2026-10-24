//! Coordinated replacement: remove tracks from the phone, prove they are
//! gone, then add the sources again with the ordinary sync, all under one
//! journal coordination.
//!
//! - `sync DIR --replace-library` (ReplaceLibrary) removes every track on the
//!   phone and adds the selected sources again.
//! - `sync DIR --replace-changed` (ReplaceTrack) removes the tracks this tool
//!   inserted whose source audio has since changed at the same path, and adds
//!   the new audio.
//!
//! The coordination's phase records progress: Removing, then Adding once a
//! fresh capture shows none of the selected tracks. Both phases work from a
//! fresh capture, so running the same command again continues an Active
//! coordination: removal takes out what is left and the sync adds only what
//! is missing. The coordination completes only when a fresh add-only plan
//! has nothing left to add and no cover of an added track pending. An
//! unfinished batch needs `resume` first.
//!
//! A whole-library replacement starts only when this tool can add every
//! selected source; one it never could would leave the phone short after the
//! removal. Since that removal takes every track whatever the selection, an
//! Active one may continue with more `--exclude` paths than it began with.
//! Removed tracks lose their identities, play counts, ratings and playlist
//! memberships; the preview lists them.
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::identity;
use music_protocol::protocol::trace::private_write;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// A coordination begins in phase 0, removing the selected tracks. Phase 1:
/// none of them is on the phone any more; adding the sources.
const ADDING: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Remove every track, then add the whole selection again.
    Library,
    /// Remove the managed tracks whose source audio changed, or whose source
    /// cleared a field an edit cannot clear, then add their sources again.
    Changed,
}
impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Changed => "changed",
        }
    }
}

/// One phone track the replacement removes.
#[derive(Clone, Debug, Serialize)]
pub struct Removal {
    pub track: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub plays: u32,
    /// The source that replaces it (changed audio only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// What the user confirms.
pub struct Preview<'a> {
    pub kind: Kind,
    /// An Active coordination this run continues; nothing new is confirmed.
    pub continuing: Option<i64>,
    pub removals: &'a [Removal],
    pub sources: usize,
    pub selection: &'a Path,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to replace (changed audio only).
    Nothing,
    Preview,
    Cancelled,
    Completed {
        coordination: i64,
        removed: usize,
        tracks: usize,
    },
}

/// The coordination's exact intent; continuing requires the same source and
/// selection, except that a whole-library replacement may exclude more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Intent {
    schema: String,
    kind: String,
    source: PathBuf,
    recursive: bool,
    include: BTreeSet<String>,
    exclude: BTreeSet<String>,
    /// Changed audio only: the tracks removed and the sources that replace them.
    tracks: Vec<String>,
    sources: Vec<String>,
}

/// An unfinished removal batch by name: a session's removal half names its
/// session.
pub(crate) fn session_or_removal(batch: &crate::engine::state::journal::BatchRecord) -> String {
    match batch.with_insert_batch {
        Some(insert) => format!(
            "the session of removal batch {} and insert batch {}",
            batch.id.as_i64(),
            insert.as_i64()
        ),
        None => format!("removal batch {}", batch.id.as_i64()),
    }
}

/// The managed tracks only a replacement brings up to date in a full plan
/// (not add-only), as (track id, source relative path): those whose source
/// audio changed at their owned path, and those whose source clears a field
/// an edit cannot clear (`metadata_update_needs_replacement`).
pub(crate) fn replacements(
    plan: &crate::engine::planning::additive::Plan,
) -> Result<Vec<(String, String)>> {
    plan.sources
        .iter()
        .filter_map(|row| {
            if let crate::engine::planning::additive::Decision::Skip {
                code: "metadata_update_needs_replacement",
                ..
            } = row.decision
            {
                return Some(match &row.matches[..] {
                    [owned] if owned.ownership == "inserted_by_us" => {
                        Ok((owned.track_id.clone(), row.relative_path.clone()))
                    }
                    _ => Err(anyhow::anyhow!(
                        "{} names no single track this tool inserted",
                        row.relative_path
                    )),
                });
            }
            let replacement = row
                .managed_audio_replacement
                .as_ref()
                .filter(|d| d.replacement_identity_established)?;
            Some(
                replacement
                    .evidence
                    .as_ref()
                    .and_then(|e| e["immutable_original_content"]["track_id"].as_str())
                    .map(|id| (id.to_owned(), row.relative_path.clone()))
                    .context("replacement evidence names no original track"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(kind: &str) -> Intent {
        Intent {
            schema: "rustunes.replacement-intent".into(),
            kind: kind.into(),
            source: "/home/u/Music/ALAC".into(),
            recursive: true,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
            tracks: Vec::new(),
            sources: Vec::new(),
        }
    }
}
