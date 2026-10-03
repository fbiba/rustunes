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

/// Every ordinary playlist that holds a target, with its members in position
/// order. Each must have the observed shape: positions 0..n, one row per
/// member and item, music members only, its count row and live revision.
fn affected(before: &Tables, removed: &BTreeSet<i64>) -> Result<BTreeMap<i64, Vec<Member>>> {
    let Some(members) = before.get("container_item") else {
        return Ok(BTreeMap::new());
    };
    let playlists = ordinary(before)?;
    let mut lists: BTreeMap<i64, Vec<(i64, Member)>> = BTreeMap::new();
    for (row, n) in &members.rows {
        let pid = typed::int(members, row, "container_pid")?;
        if !playlists.contains(&pid) {
            continue;
        }
        ensure!(*n == 1, "playlist {pid} holds a duplicate membership row");
        lists.entry(pid).or_default().push((
            typed::int(members, row, "position")?,
            Member {
                row: row.clone(),
                member: typed::int(members, row, "container_item_pid")?,
                item: typed::int(members, row, "item_pid")?,
            },
        ));
    }
    let mut affected = BTreeMap::new();
    for (pid, mut list) in lists {
        if !list.iter().any(|(_, m)| removed.contains(&m.item)) {
            continue;
        }
        let containers = table(before, "container")?;
        let counts = table(before, "container_item_media_type")?;
        list.sort_by_key(|(position, _)| *position);
        let mut items = BTreeSet::new();
        for (index, (position, m)) in list.iter().enumerate() {
            ensure!(
                *position == index as i64
                    && m.member > 0
                    && items.insert(m.item)
                    && members.at(&m.row, "uuid")?.text()?.is_empty()
                    && members.at(&m.row, "position_uuid")?.text()?.is_empty()
                    && members.at(&m.row, "occurrence_id")?.text()? == format!("{}_0", m.item),
                "playlist {pid} has an unmodelled membership shape"
            );
        }
        let count = counts.one("container_pid", pid)?;
        ensure!(
            typed::int(counts, count, "media_type")? == MUSIC
                && typed::int(counts, count, "count")? == list.len() as i64
                && typed::int(
                    containers,
                    containers.one("container_pid", pid)?,
                    "contained_media_type"
                )? == MUSIC,
            "playlist {pid} holds more than its music members"
        );
        let revisions = own_revisions(before, pid)?;
        ensure!(
            revisions.contains_key(&0),
            "playlist {pid} has no live revision"
        );
        affected.insert(pid, list.into_iter().map(|(_, m)| m).collect());
    }
    Ok(affected)
}

/// The ordinary playlists that hold an item of `removed`.
pub fn holding(before: &Tables, removed: &BTreeSet<i64>) -> Result<BTreeSet<i64>> {
    let mut holding = BTreeSet::new();
    let Some(members) = before.get("container_item") else {
        return Ok(holding);
    };
    let playlists = ordinary(before)?;
    for row in members.rows.keys() {
        let pid = typed::int(members, row, "container_pid")?;
        if playlists.contains(&pid) && removed.contains(&typed::int(members, row, "item_pid")?) {
            holding.insert(pid);
        }
    }
    Ok(holding)
}

/// Before-only check for `qualify`: every playlist a target belongs to has
/// the modelled shape.
pub(super) fn qualify(before: &Tables, removed: &BTreeSet<i64>) -> Result<()> {
    affected(before, removed).map(drop)
}

/// The deterministic effects on memberships, counts and container rows.
pub(super) fn remove_memberships(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
    removed: &BTreeSet<i64>,
) -> Result<Vec<PlaylistRemoval>> {
    let mut removals = Vec::new();
    for (pid, list) in affected(before, removed)? {
        let members = table(before, "container_item")?;
        let mut removed_members = Vec::new();
        let mut moved_members = 0;
        let mut position = 0;
        for m in &list {
            if removed.contains(&m.item) {
                ensure!(
                    expected
                        .get_mut("container_item")
                        .context("memberships absent")?
                        .rows
                        .remove(&m.row)
                        == Some(1),
                    "playlist {pid} membership {} is not unique",
                    m.member
                );
                removed_members.push(m.member);
                continue;
            }
            if typed::int(members, &m.row, "position")? != position {
                let mut desired = m.row.clone();
                typed::set(members, &mut desired, "position", Cell::Integer(position))?;
                typed::replace(expected, "container_item", &m.row, desired)?;
                moved_members += 1;
            }
            position += 1;
        }
        let emptied = position == 0;
        let counts = table(before, "container_item_media_type")?;
        let count = counts.one("container_pid", pid)?.clone();
        if emptied {
            expected
                .get_mut("container_item_media_type")
                .context("media counts absent")?
                .rows
                .remove(&count);
        } else {
            let mut desired = count.clone();
            typed::set(counts, &mut desired, "count", Cell::Integer(position))?;
            typed::replace(expected, "container_item_media_type", &count, desired)?;
        }
        let containers = table(before, "container")?;
        let row = containers.one("container_pid", pid)?.clone();
        let mut desired = row.clone();
        if emptied {
            typed::set(
                containers,
                &mut desired,
                "contained_media_type",
                Cell::Integer(0),
            )?;
        }
        let mut keep_local_evaluated = false;
        if typed::int(containers, &row, "keep_local_status")? == 0 {
            let mut evaluated = desired.clone();
            typed::set(
                containers,
                &mut evaluated,
                "keep_local_status",
                Cell::Integer(2),
            )?;
            if table(after, "container")?.one("container_pid", pid).ok() == Some(&evaluated) {
                desired = evaluated;
                keep_local_evaluated = true;
            }
        }
        if desired != row {
            typed::replace(expected, "container", &row, desired)?;
        }
        removals.push(PlaylistRemoval {
            playlist_pid: pid,
            members_before: list.len(),
            removed_members,
            moved_members,
            emptied,
            revision: (0, 0),
            removed_membership_revision: None,
            keep_local_revision: None,
            keep_local_evaluated,
        });
    }
    Ok(removals)
}

/// Ordinary playlists outside `removals` still at keep_local_status 0 whose
/// after row is the before row with status 2. `expected` takes the status and
/// loses the old membership revision; `add_keep_local` adds the new one.
pub(super) fn evaluate_untouched(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
    removals: &[PlaylistRemoval],
) -> Result<Vec<KeepLocalEvaluation>> {
    let mut evaluated = Vec::new();
    let Some(containers) = before.get("container") else {
        return Ok(evaluated);
    };
    let after_containers = table(after, "container")?;
    for pid in ordinary(before)? {
        if removals.iter().any(|r| r.playlist_pid == pid) {
            continue;
        }
        let row = containers.one("container_pid", pid)?.clone();
        if typed::int(containers, &row, "keep_local_status")? != 0 {
            continue;
        }
        let mut desired = row.clone();
        typed::set(
            containers,
            &mut desired,
            "keep_local_status",
            Cell::Integer(2),
        )?;
        if after_containers.one("container_pid", pid).ok() != Some(&desired) {
            continue;
        }
        typed::replace(expected, "container", &row, desired)?;
        let mut removed_membership_revision = None;
        if let Some((membership, number)) = own_revisions(before, pid)?.get(&1) {
            expected
                .get_mut("entity_revision")
                .context("revisions absent")?
                .rows
                .remove(membership);
            removed_membership_revision = Some(*number);
        }
        evaluated.push(KeepLocalEvaluation {
            playlist_pid: pid,
            removed_membership_revision,
            keep_local_revision: None,
        });
    }
    Ok(evaluated)
}

/// Each affected playlist's live revision moves to a new number above the
/// last synced revision, distinct from every other number the session wrote
/// (`numbers`, extended here); its membership revision goes.
pub(super) fn move_revisions(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
    removals: &mut [PlaylistRemoval],
    last_synced: i64,
    numbers: &mut BTreeSet<i64>,
) -> Result<()> {
    for removal in removals {
        let pid = removal.playlist_pid;
        let old = own_revisions(before, pid)?;
        let now = own_revisions(after, pid)?;
        let (old_live, old_number) = old.get(&0).context("playlist revision missing")?;
        let (new_live, new_number) = now
            .get(&0)
            .with_context(|| format!("playlist {pid} has no live revision after the removal"))?;
        let revisions = table(before, "entity_revision")?;
        let mut desired = old_live.clone();
        typed::set(
            revisions,
            &mut desired,
            "revision",
            Cell::Integer(*new_number),
        )?;
        ensure!(
            *new_number > *old_number
                && *new_number > last_synced
                && numbers.insert(*new_number)
                && desired == *new_live,
            "playlist {pid} revision is invalid"
        );
        typed::replace(expected, "entity_revision", old_live, desired)?;
        removal.revision = (*old_number, *new_number);
        if let Some((membership, number)) = old.get(&1) {
            expected
                .get_mut("entity_revision")
                .context("revisions absent")?
                .rows
                .remove(membership);
            removal.removed_membership_revision = Some(*number);
        }
        ensure!(
            now.contains_key(&1) == removal.keep_local_evaluated,
            "playlist {pid} membership revision is unmodelled"
        );
    }
    Ok(())
}

/// The membership revisions of keep-local evaluations, which lie above every
/// number the session wrote. They are taken out of `actual` so the survivor
/// refresh compares like with like; `add_keep_local` puts them back.
pub(super) fn keep_local_rows(
    after: &Tables,
    removals: &[PlaylistRemoval],
    evaluated: &[KeepLocalEvaluation],
) -> Result<Vec<(i64, Row, i64)>> {
    let pids = removals
        .iter()
        .filter(|r| r.keep_local_evaluated)
        .map(|r| r.playlist_pid)
        .chain(evaluated.iter().map(|e| e.playlist_pid));
    let mut rows = Vec::new();
    for pid in pids {
        let now = own_revisions(after, pid)?;
        let (row, number) = now
            .get(&1)
            .with_context(|| format!("playlist {pid} keep-local revision missing"))?;
        rows.push((pid, row.clone(), *number));
    }
    Ok(rows)
}

pub(super) fn add_keep_local(
    expected: &mut Tables,
    removals: &mut [PlaylistRemoval],
    evaluated: &mut [KeepLocalEvaluation],
    rows: Vec<(i64, Row, i64)>,
    written: i64,
) -> Result<()> {
    let mut numbers = BTreeSet::new();
    for (pid, row, number) in rows {
        ensure!(
            number > written && numbers.insert(number),
            "playlist {pid} keep-local revision is not above the session's revisions"
        );
        let revisions = expected
            .get_mut("entity_revision")
            .context("revisions absent")?;
        ensure!(
            revisions.rows.insert(row, 1).is_none(),
            "duplicate keep-local revision"
        );
        // The new membership revision replaces the old one, never keeps it.
        let replaced = match (
            removals.iter_mut().find(|r| r.playlist_pid == pid),
            evaluated.iter_mut().find(|e| e.playlist_pid == pid),
        ) {
            (Some(removal), _) => {
                removal.keep_local_revision = Some(number);
                removal.removed_membership_revision
            }
            (None, Some(evaluation)) => {
                evaluation.keep_local_revision = Some(number);
                evaluation.removed_membership_revision
            }
            (None, None) => bail!("playlist {pid} keep-local revision is unmodelled"),
        };
        ensure!(
            replaced.is_none_or(|old| number > old),
            "playlist {pid} keep-local revision does not replace its membership revision"
        );
    }
    Ok(())
}

/// A container's live class-1 revisions by type: 0 (live) and optionally 1
/// (membership). Any other type, a duplicate or a tombstone refuses.
pub(super) fn own_revisions(tables: &Tables, pid: i64) -> Result<BTreeMap<i64, (Row, i64)>> {
    let revisions = table(tables, "entity_revision")?;
    let mut own = BTreeMap::new();
    for (row, n) in &revisions.rows {
        if typed::int(revisions, row, "entity_pid")? != pid
            || typed::int(revisions, row, "class")? != CLASS
        {
            continue;
        }
        let kind = typed::int(revisions, row, "revision_type")?;
        if !matches!(kind, 0 | 1) {
            bail!("playlist {pid} has an unmodelled revision type {kind}");
        }
        ensure!(
            *n == 1
                && typed::int(revisions, row, "deleted")? == 0
                && own
                    .insert(kind, (row.clone(), typed::int(revisions, row, "revision")?))
                    .is_none(),
            "playlist {pid} has an ambiguous or deleted revision"
        );
    }
    Ok(own)
}
