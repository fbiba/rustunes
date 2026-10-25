//! A Started single insert or cover, observed fresh before any other route.
//!
//! The snapshot `resume` is given is normally the run's own Before, where the
//! row an interrupted insert left without audio never appears, and an
//! attempt the phone never applied looked like any other uncertain one. A
//! complete capture taken now decides: the anchor still at the prepared
//! revision, nothing of the attempt in the catalogue, no file at its planned
//! path and nothing staged (or only the attempt's own pair, never finished,
//! which is removed) settles it NotApplied; an insert's row without a
//! file location is compensated; a cover whose session ended after the phone
//! took its metadata and before its image arrived is settled Abandoned;
//! anything else goes to the attempt's own route, which verifies what the
//! phone holds. Nothing is ever resent.
use super::*;
use crate::engine::recovery::retained;
use crate::{
    engine::execution::native_insert::{create_trace, durable_write},
    engine::recovery::stale_staging,
    engine::state::audio_index::Absent,
    engine::state::journal::{Attempt, AttemptKind},
};
use anyhow::ensure;
use music_protocol::catalogue::identity;
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::protocol::transfer::{MAX_BOUNDED_OPS, Transfer, hex};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, time::Duration};

const STAGING: &str = "iTunes_Control/Sync/Media";
const AIRLOCK: &str = "Airlock/Media";
const ARTWORK: &str = "Airlock/Media/Artwork";

/// What the fresh catalogue shows of a Started insert or cover.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// The anchor never moved and nothing of the attempt is in the catalogue;
    /// the phone checks (no file, nothing staged) come next.
    Unmoved,
    /// The insert's row is in the catalogue without a file location.
    Locationless,
    /// The cover's session moved the anchor by one and gave the track, still
    /// at its path, the new cache ID, but no image arrived for it; the
    /// transcript and staging checks come next.
    Imageless,
    /// The phone took a session: the attempt's own route verifies it.
    Moved,
}

/// `before` is the snapshot `resume` was given, used only when it holds the
/// prepared revision: then an unmoved anchor must also keep its tracks.
pub(super) fn classify(attempt: &Attempt, fresh: &Catalog, before: &Catalog) -> Result<Seen> {
    let prepared = &attempt.prepared;
    let content = &prepared.content;
    let row = fresh
        .tracks()
        .iter()
        .find(|t| t.sync_id == content.track_pid);
    let same_tracks = || {
        let ids = |c: &Catalog| {
            c.tracks()
                .iter()
                .map(|t| t.sync_id)
                .collect::<BTreeSet<_>>()
        };
        before.persisted_media_anchor() != prepared.current_media_anchor
            || ids(before) == ids(fresh)
    };
    if fresh.persisted_media_anchor() != prepared.current_media_anchor {
        let imageless = || {
            content.artwork.as_ref().is_some_and(|art| {
                fresh.persisted_media_anchor() == prepared.current_media_anchor + 1
                    && row.is_some_and(|r| r.afc_path.as_ref() == Some(&content.path))
                    && fresh.artwork().track_cache_ids().get(&content.track_pid)
                        == Some(&art.cache_id)
                    && !fresh.artwork().cached_ids().contains(&art.cache_id)
                    && same_tracks()
            })
        };
        return Ok(match prepared.kind {
            AttemptKind::Insert if row.is_some_and(|r| r.afc_path.is_none()) => Seen::Locationless,
            AttemptKind::Artwork if imageless() => Seen::Imageless,
            _ => Seen::Moved,
        });
    }
    let id = attempt.id.as_i64();
    match prepared.kind {
        AttemptKind::Insert => ensure!(
            row.is_none(),
            "the anchor did not move but track {:016x} is in the catalogue; attempt {id} stays Started",
            content.track_pid
        ),
        AttemptKind::Artwork => {
            let art = content
                .artwork
                .as_ref()
                .context("prepared artwork missing")?;
            ensure!(
                row.is_some()
                    && fresh.artwork().track_cache_ids().get(&content.track_pid)
                        != Some(&art.cache_id)
                    && !fresh.artwork().occupied_cache_ids().contains(&art.cache_id),
                "the anchor did not move but the catalogue holds cover {} of track {:016x}; attempt {id} stays Started",
                art.cache_id,
                content.track_pid
            );
        }
    }
    ensure!(
        same_tracks(),
        "the anchor did not move but the catalogue's tracks changed; attempt {id} stays Started"
    );
    Ok(Seen::Unmoved)
}
