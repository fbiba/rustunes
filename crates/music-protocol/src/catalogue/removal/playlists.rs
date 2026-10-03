//! Ordinary playlists lose a removed track's membership. In one session, for
//! each playlist that held a target: every target's membership row goes, the
//! surviving members close up in order from position 0 and keep their member
//! IDs, the media count follows (its row goes when no member is left) and an
//! emptied playlist's media type becomes 0. The playlist's live revision moves
//! to a new number above the last synced revision, among the session's
//! tombstones, and its membership revision goes. Any ordinary playlist the
//! phone had not yet evaluated for keeping local (keep_local_status 0) may be
//! evaluated in the same session, whether or not it held a target: the status
//! becomes 2 and its membership revision is replaced by one above every
//! revision the session wrote (seen for a playlist that lost a member, and for
//! one created minutes before that held none). Library-container and Playback
//! History memberships stay (see the parent module).
use super::{Tables, table};
use crate::catalogue::sqlite::tables::{Cell, Row, values as typed};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// entity_revision class of a container.
pub(super) const CLASS: i64 = 1;
/// Media type of every observed member: a local music track.
const MUSIC: i64 = 8;
/// Container columns that are zero in an ordinary playlist.
const ORDINARY_ZERO: [&str; 12] = [
    "distinguished_kind",
    "container_type",
    "is_hidden",
    "parent_pid",
    "smart_is_folder",
    "smart_is_dynamic",
    "smart_is_filtered",
    "smart_is_genius",
    "is_external_vendor_playlist",
    "is_collaborative",
    "cloud_is_subscribed",
    "store_cloud_id",
];

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PlaylistRemoval {
    pub(super) playlist_pid: i64,
    pub(super) members_before: usize,
    /// Member IDs of the targets' rows, in position order.
    pub(super) removed_members: Vec<i64>,
    /// Survivors whose position changed when the list closed up.
    pub(super) moved_members: usize,
    pub(super) emptied: bool,
    pub(super) revision: (i64, i64),
    pub(super) removed_membership_revision: Option<i64>,
    /// The new membership revision when the phone evaluated keep-local.
    pub(super) keep_local_revision: Option<i64>,
    #[serde(skip)]
    keep_local_evaluated: bool,
}

/// An ordinary playlist that held no target, evaluated for keeping local in
/// the session.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct KeepLocalEvaluation {
    pub(super) playlist_pid: i64,
    pub(super) removed_membership_revision: Option<i64>,
    pub(super) keep_local_revision: Option<i64>,
}

/// Ordinary playlists a computer synced to the phone: not smart, not a
/// folder, not hidden or distinguished, not the library container, and
/// without a cloud identity.
pub fn ordinary(before: &Tables) -> Result<BTreeSet<i64>> {
    let mut ordinary = BTreeSet::new();
    let (Some(containers), Some(info)) = (before.get("container"), before.get("db_info")) else {
        return Ok(ordinary);
    };
    let mut primary = BTreeSet::new();
    for row in info.rows.keys() {
        primary.insert(typed::int(info, row, "primary_container_pid")?);
    }
    for row in containers.rows.keys() {
        let pid = typed::int(containers, row, "container_pid")?;
        let mut plain = pid != 0
            && !primary.contains(&pid)
            && typed::int(containers, row, "sync_id")? == pid
            && typed::int(containers, row, "is_src_remote")? == 1
            && *containers.at(row, "smart_criteria")? == Cell::Null
            && containers.at(row, "cloud_global_id")?.text()?.is_empty();
        for key in ORDINARY_ZERO {
            plain &= typed::int(containers, row, key)? == 0;
        }
        if plain {
            ordinary.insert(pid);
        }
    }
    Ok(ordinary)
}

struct Member {
    row: Row,
    member: i64,
    item: i64,
}
