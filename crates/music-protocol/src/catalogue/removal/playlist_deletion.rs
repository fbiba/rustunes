//! Expected complete SQL effect of one native metadata-only session that
//! deletes ordinary empty playlists, one `delete_playlist` each after
//! `update_db_info`. For each playlist: its
//! `container` row and its `container_seed` row go, and its class-1 revisions
//! (live, and membership when present) become one tombstone: the live row with
//! `deleted` 2 at a new number above the last synced revision. The tombstones
//! rise in operation order. Sync properties advance by one anchor with the
//! highest tombstone as last synced; hero progress follows it or stays. As in
//! any session, recent play/skip counters may reset, the maintenance clock
//! may advance and an entry the phone added itself may have its state filled
//! in. Every other typed row in all tables must be exact.
//!
//! Only empty playlists qualify: deleting one with members leaves its media
//! count row behind, so a Clear deletes its playlists after every
//! track is gone. A playlist anything else refers to (members, a count row,
//! child playlists, artwork, any other table's `container_pid`) refuses.
use super::{
    ANCHOR, BetweenRuns, GENERATION, HERO, LAST_SYNCED, Tables, decimal, fill_phone_entries,
    playlists, property, property_changes, recent_counter_resets, table,
};
use crate::catalogue::sqlite::tables::{Cell, Row, values as typed};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Tables the deletion itself changes.
const OWN: [&str; 2] = ["container", "container_seed"];

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DeletedPlaylist {
    playlist_pid: i64,
    removed_seed_rows: u32,
    /// Live revision before and the tombstone's number after.
    revision: (i64, i64),
    removed_membership_revision: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct PlaylistDeletionReceipt {
    schema: &'static str,
    playlists: Vec<DeletedPlaylist>,
    property_changes: BTreeMap<&'static str, (String, String)>,
    recent_counter_resets: Vec<i64>,
    /// Entries the phone added itself whose state the session filled in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filled_phone_entries: Vec<i64>,
    /// What a later run's capture showed of the phone's use since the
    /// session (`verify_deletion_later`).
    #[serde(skip_serializing_if = "Option::is_none")]
    between_runs_playback: Option<serde_json::Value>,
}

impl PlaylistDeletionReceipt {
    pub fn deleted(&self) -> usize {
        self.playlists.len()
    }
}

/// Before-only qualification, run before sending and again at verification:
/// each playlist is a distinct ordinary playlist with no member, no media
/// count, no child, no artwork and no other reference, at most its one seed
/// sentinel, and a live revision.
pub fn qualify_deletion(before: &Tables, pids: &[i64]) -> Result<()> {
    qualify(before, pids, false)
}

/// The same checks while the Clear's tracks are still there: members and
/// their media count are allowed, since the track batches take them out
/// (`playlists`); every other reference refuses.
pub fn qualify_clear(before: &Tables, pids: &[i64]) -> Result<()> {
    qualify(before, pids, true)
}

fn qualify(before: &Tables, pids: &[i64], members: bool) -> Result<()> {
    ensure!(!pids.is_empty(), "playlist deletion has no playlists");
    let distinct: BTreeSet<i64> = pids.iter().copied().collect();
    ensure!(
        distinct.len() == pids.len() && !distinct.contains(&0),
        "deleted playlists must be distinct nonzero containers"
    );
    let ordinary = playlists::ordinary(before)?;
    if let Some(pid) = distinct.iter().find(|p| !ordinary.contains(p)) {
        bail!("playlist {pid} is not an ordinary synced playlist");
    }
    let containers = table(before, "container")?;
    for pid in &distinct {
        let row = containers.one("container_pid", *pid)?;
        ensure!(
            members || typed::int(containers, row, "contained_media_type")? == 0,
            "playlist {pid} is not empty"
        );
    }
    for row in containers.rows.keys() {
        let parent = typed::int(containers, row, "parent_pid")?;
        ensure!(
            !distinct.contains(&parent),
            "playlist {parent} holds another playlist"
        );
    }
    for (name, t) in before {
        if name == "container" {
            continue;
        }
        if let Ok(column) = t.column("container_pid") {
            for row in t.rows.keys() {
                let Cell::Integer(pid) = row[column] else {
                    continue;
                };
                if !distinct.contains(&pid)
                    || members
                        && matches!(
                            name.as_str(),
                            "container_item" | "container_item_media_type"
                        )
                {
                    continue;
                }
                ensure!(
                    name == "container_seed"
                        && typed::int(t, row, "item_pid")? == 0
                        && typed::int(t, row, "seed_order")? == 0,
                    "playlist {pid} has an unmodelled {name} row"
                );
            }
        }
        if let Ok(column) = t.column("entity_pid") {
            if name == "entity_revision" {
                continue;
            }
            for row in t.rows.keys() {
                if let Cell::Integer(pid) = row[column] {
                    ensure!(
                        !distinct.contains(&pid),
                        "playlist {pid} has an unmodelled {name} row"
                    );
                }
            }
        }
    }
    for pid in &distinct {
        ensure!(
            playlists::own_revisions(before, *pid)?.contains_key(&0),
            "playlist {pid} has no live revision"
        );
    }
    for key in [ANCHOR, LAST_SYNCED, GENERATION, HERO] {
        property(before, key)?;
    }
    Ok(())
}

pub fn verify_deletion(
    before: &Tables,
    after: &Tables,
    pids: &[i64],
    base_anchor: u32,
) -> Result<PlaylistDeletionReceipt> {
    verify_deletion_with(before, after, pids, base_anchor, None)
}

/// `verify_deletion` for a later run's capture, after the phone may have
/// been used: what the deletion wrote stays exact, and `between_runs`
/// decides the rest of the difference above the highest tombstone.
pub fn verify_deletion_later(
    before: &Tables,
    after: &Tables,
    pids: &[i64],
    base_anchor: u32,
    between_runs: BetweenRuns<'_>,
) -> Result<PlaylistDeletionReceipt> {
    verify_deletion_with(before, after, pids, base_anchor, Some(between_runs))
}

fn verify_deletion_with(
    before: &Tables,
    after: &Tables,
    pids: &[i64],
    base_anchor: u32,
    between_runs: Option<BetweenRuns<'_>>,
) -> Result<PlaylistDeletionReceipt> {
    qualify_deletion(before, pids)?;
    ensure!(
        before.keys().eq(after.keys())
            && before.iter().all(|(n, t)| {
                t.columns == after[n].columns && t.definitions == after[n].definitions
            }),
        "playlist deletion table set or column definitions differ"
    );
    ensure!(
        decimal(property(before, ANCHOR)?, false)? == i64::from(base_anchor),
        "playlist deletion before state is not at the prepared anchor"
    );
    let mut expected = before.clone();
    let last_synced = decimal(property(before, LAST_SYNCED)?, false)?;
    let revisions = table(before, "entity_revision")?;
    let mut previous = last_synced;
    let mut deleted = Vec::new();
    for pid in pids {
        let mut removed_seed_rows = 0;
        for name in OWN {
            let t = expected.get_mut(name).context("container table absent")?;
            let key = t.column("container_pid")?;
            let mut count = 0;
            t.rows.retain(|r, n| {
                let own = r[key] == Cell::Integer(*pid);
                if own {
                    count += *n;
                }
                !own
            });
            match name {
                "container" => ensure!(count == 1, "playlist {pid} row is not unique"),
                _ => removed_seed_rows = count,
            }
        }
        let old = playlists::own_revisions(before, *pid)?;
        let (live, number) = old.get(&0).context("playlist revision missing")?;
        let now = class_rows(after, *pid)?;
        ensure!(
            now.len() == 1,
            "playlist {pid} has {} revisions after its deletion",
            now.len()
        );
        let tombstone = &now[0];
        let after_number = typed::int(revisions, tombstone, "revision")?;
        let mut desired = live.clone();
        typed::set(
            revisions,
            &mut desired,
            "revision",
            Cell::Integer(after_number),
        )?;
        typed::set(revisions, &mut desired, "deleted", Cell::Integer(2))?;
        ensure!(
            after_number > *number && after_number > previous && desired == *tombstone,
            "playlist {pid} tombstone is invalid"
        );
        previous = after_number;
        typed::replace(&mut expected, "entity_revision", live, desired)?;
        let mut removed_membership_revision = None;
        if let Some((membership, number)) = old.get(&1) {
            expected
                .get_mut("entity_revision")
                .context("revisions absent")?
                .rows
                .remove(membership);
            removed_membership_revision = Some(*number);
        }
        deleted.push(DeletedPlaylist {
            playlist_pid: *pid,
            removed_seed_rows,
            revision: (*number, after_number),
            removed_membership_revision,
        });
    }
    let highest = previous;
    let recent_counter_resets =
        recent_counter_resets(before, after, &mut expected, &BTreeSet::new())?;
    let filled_phone_entries = fill_phone_entries(before, after, &mut expected)?;
    let anchor = base_anchor
        .checked_add(1)
        .context("playlist deletion anchor exhausted")?;
    let hero = decimal(property(after, HERO)?, false)?;
    ensure!(
        decimal(property(after, ANCHOR)?, false)? == i64::from(anchor)
            && decimal(property(after, LAST_SYNCED)?, false)? == highest,
        "playlist deletion anchor/last-synced properties differ"
    );
    ensure!(
        hero == highest || hero == decimal(property(before, HERO)?, false)?,
        "playlist deletion hero revision progress differs"
    );
    let property_changes = property_changes(before, after, &mut expected)?;
    let between_runs_playback = match between_runs {
        Some(absorb) => absorb(&mut expected, after, highest)?,
        None => None,
    };
    let mismatches: Vec<_> = expected
        .iter()
        .filter(|(n, t)| t.rows != after[*n].rows)
        .map(|(n, _)| n.as_str())
        .collect();
    ensure!(
        mismatches.is_empty(),
        "unmodelled playlist deletion rows in tables: {}",
        mismatches.join(", ")
    );
    Ok(PlaylistDeletionReceipt {
        schema: "music-protocol.playlist-deletion-sql",
        playlists: deleted,
        property_changes,
        recent_counter_resets,
        filled_phone_entries,
        between_runs_playback,
    })
}

/// True when the anchor did not move and every playlist's row and revisions
/// are as before: the session demonstrably did not apply.
pub fn deletion_not_applied(before: &Tables, after: &Tables, pids: &[i64]) -> Result<bool> {
    if property(before, ANCHOR)? != property(after, ANCHOR)? {
        return Ok(false);
    }
    for pid in pids {
        let own = |t: &Tables| -> Result<Vec<Row>> {
            let containers = table(t, "container")?;
            let key = containers.column("container_pid")?;
            let mut rows: Vec<Row> = containers
                .rows
                .keys()
                .filter(|r| r[key] == Cell::Integer(*pid))
                .cloned()
                .collect();
            rows.extend(class_rows(t, *pid)?);
            Ok(rows)
        };
        let old = own(before)?;
        if old.is_empty() || old != own(after)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every class-1 revision row of a container, tombstones included.
fn class_rows(tables: &Tables, pid: i64) -> Result<Vec<Row>> {
    let revisions = table(tables, "entity_revision")?;
    let mut rows = Vec::new();
    for (row, n) in &revisions.rows {
        if typed::int(revisions, row, "entity_pid")? == pid
            && typed::int(revisions, row, "class")? == playlists::CLASS
        {
            ensure!(*n == 1, "playlist {pid} has a duplicate revision");
            rows.push(row.clone());
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
