//! Expected complete SQL effect of one native metadata-only session that
//! removes one or more catalogue rows, one `delete_track` per target.
//!
//! Observed receiver shape (the locationless row an interrupted insert left;
//! four ordinary tracks in one session): every target item row goes, its live
//! revision becomes a tombstone above the last synced revision, its stats
//! revision row goes, sync properties advance by one anchor, its own artwork
//! token rows go, and group rows stay with representatives that point at a
//! surviving member or zero. Membership rows in the library container and
//! Playback History stay. Every other typed row in all tables must be exact.
//! Later, the phone may recompute an album the removal emptied (observed for
//! that locationless row): its keep_local_status becomes 2 and its album
//! revision moves directly above the tombstones; its stats revision row, if it
//! has one, goes with it. An album an earlier session left unevaluated
//! (keep_local_status 0: an insert batch killed mid-upload leaves its new album
//! so) may be evaluated the same way though it keeps members: status 2, its
//! revision moved directly above the tombstones, its stats revision row gone,
//! as a metadata edit evaluated one. A session may instead collect every group
//! no item belongs to any more (observed for 100 targets): each such group row
//! goes, its live revision becomes a tombstone above the item tombstones, its
//! stats revision row goes, and its artwork token rows stay. The next session
//! deletes such token rows, whose entity was already gone when it began. A
//! session also moves an artist's or a genre's representative off a member
//! without a cover to its lowest member with one. A target's memberships in
//! ordinary playlists go, and those playlists close up (`playlists`); any
//! ordinary playlist not yet evaluated for keeping local may be evaluated in
//! the session. An entry the phone added itself, which no sync made, may have
//! its state filled in as a synced track's (`fill_phone_entries`). A mismatch
//! refuses; the caller retains the full delta for review.
use crate::catalogue::sqlite::{
    PLAYBACK_COLUMNS,
    tables::{Cell, Row, Rows, Table, values as typed},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub type Tables = BTreeMap<String, Table>;

/// What a later run's capture may hold besides the session: the phone was
/// used in between (plays and skips, Playback History, its own renumbering of
/// live items, superseded statistics revisions, the maintenance clock). Given
/// the model and the after tables, it proves that the rest of their
/// difference is only that, takes it into the model and reports it; a
/// renumbered or fresh revision must lie above `floor`, the session's
/// highest tombstone. The caller supplies the rule.
pub type BetweenRuns<'a> =
    &'a dyn Fn(&mut Tables, &Tables, i64) -> Result<Option<serde_json::Value>>;

const ITEM_TABLES: [&str; 10] = [
    "item",
    "item_extra",
    "item_store",
    "item_stats",
    "item_playback",
    "item_search",
    "item_state",
    "item_video",
    "chapter",
    "lyrics",
];
/// Group table, its item column and its entity_revision class.
const GROUPS: [(&str, &str, i64); 5] = [
    ("album", "album_pid", ALBUM_CLASS),
    ("album_artist", "album_artist_pid", 7),
    ("item_artist", "item_artist_pid", 2),
    ("genre", "genre_id", 3),
    ("composer", "composer_pid", 5),
];
const ANCHOR: &str = "MLSyncClientSyncAnchor";
const LAST_SYNCED: &str = "MLSyncClientLastSyncedRevision";
const GENERATION: &str = "MLSyncClientGenerationID";
const HERO: &str = "MLArtistHeroImageImportDatabaseRevision";
const MAINTENANCE: &str = "MLMaintenanceTaskOperationDate";
const PLAYBACK_HISTORY: i64 = 10000;
/// Entity type of an artwork token, with the table and key of its entity.
const TOKEN_ENTITIES: [(i64, &str, &str); 4] = [
    (0, "item", "item_pid"),
    (2, "item_artist", "item_artist_pid"),
    (ALBUM_CLASS, "album", "album_pid"),
    (7, "album_artist", "album_artist_pid"),
];
const ALBUM_CLASS: i64 = 4;
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RevisionRefresh {
    item_pid: i64,
    before: i64,
    after: i64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EmptiedAlbum {
    album_pid: i64,
    keep_local_status: Option<(i64, i64)>,
    revision: Option<(i64, i64)>,
    removed_stats_revision: Option<i64>,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TargetRemoval {
    item_pid: i64,
    removed_rows: BTreeMap<&'static str, u32>,
    tombstone_revision_before: i64,
    tombstone_revision_after: i64,
    removed_stats_revision: Option<i64>,
    removed_artwork_rows: u32,
    /// Library-container and Playback History rows, which stay.
    retained_memberships: u32,
}
