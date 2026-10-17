//! The removal receiver model is `music_protocol::catalogue::removal`, re-exported
//! here. The Clear command's choice of playlists stays with the command.
use crate::{
    engine::verification::sql_tables::reader::Cell, engine::verification::sql_values as typed,
};
use anyhow::{Context, Result};
use music_protocol::catalogue::removal::playlists;
use std::collections::BTreeSet;

pub(crate) use music_protocol::catalogue::removal::{
    Tables, deletion_not_applied, not_applied, not_applied_later, qualify, qualify_clear,
    qualify_deletion, qualify_session, verify, verify_deletion, verify_deletion_later,
    verify_later,
};

/// What a Clear does with playlists: `delete` holds the ordinary ones, which
/// it deletes once no track is left; `kept` every other playlist someone made
/// (smart, folder, made on the phone), which stays. The library container and
/// the phone's built-in (distinguished) containers are in neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClearPlaylists {
    pub(crate) delete: Vec<(i64, String)>,
    pub(crate) kept: Vec<(i64, String)>,
}

/// Whether an ordinary playlist holds any of `tracks` (sync IDs): a session
/// that also inserts removes none of them (`qualify_session`).
pub(crate) fn in_playlists(tables: &Tables, tracks: &[u64]) -> Result<bool> {
    let store = tables.get("item_store").context("item_store absent")?;
    let mut items = BTreeSet::new();
    for track in tracks {
        let row = typed::unique(store, "sync_id", Cell::Integer(*track as i64))?;
        items.insert(typed::int(store, row, "item_pid")?);
    }
    Ok(!playlists::holding(tables, &items)?.is_empty())
}

pub(crate) fn clear_playlists(tables: &Tables) -> Result<ClearPlaylists> {
    let mut found = ClearPlaylists::default();
    let Some(containers) = tables.get("container") else {
        return Ok(found);
    };
    let ordinary = playlists::ordinary(tables)?;
    let mut primary = BTreeSet::new();
    if let Some(info) = tables.get("db_info") {
        for row in info.rows.keys() {
            primary.insert(typed::int(info, row, "primary_container_pid")?);
        }
    }
    for row in containers.rows.keys() {
        let pid = typed::int(containers, row, "container_pid")?;
        if primary.contains(&pid) || typed::int(containers, row, "distinguished_kind")? != 0 {
            continue;
        }
        let name = containers.at(row, "name")?.text()?.to_owned();
        match ordinary.contains(&pid) {
            true => found.delete.push((pid, name)),
            false => found.kept.push((pid, name)),
        }
    }
    found.delete.sort();
    found.kept.sort();
    Ok(found)
}

#[cfg(test)]
mod clear_tests;
#[cfg(test)]
mod later_tests;
