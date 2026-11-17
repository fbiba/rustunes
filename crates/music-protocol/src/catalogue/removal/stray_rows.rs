//! Rows whose track is gone, in the tables beside `item` keyed by
//! `item_pid`, across a session and between runs.
//!
//! A property write for a track that was deleted meanwhile inserts a row in
//! the table it writes (`INSERT OR REPLACE` naming a few columns, the rest at
//! their defaults), and the `item_state_*_insert_sync` triggers add an
//! `item_state` row. The catalogue reader passes over such rows. The phone
//! changes them on its own:
//!
//! * When a task that transfers files ends or is cancelled, that is at the
//!   end of a session with file transfers (an insert), it deletes the stray
//!   `item_store` and `item_stats` rows (the sweep). A session that only
//!   stages an operation (a removal, a playlist deletion, a metadata edit)
//!   does not.
//! * On an iOS update, or once per iOS build, it deletes the stray rows of
//!   every table here but `item_state` (the sanitize). Nothing deletes a
//!   stray `item_state` row.
//! * Between runs, a write for a deleted track, a track a run removed
//!   included, can add stray rows or change them.
//! * A new track takes `max(item.item_pid) + 1`, counted over `item` alone,
//!   which may be a stray row's pid. Its import replaces the stray rows in
//!   the tables it writes and takes over the stray `item_state` row: each
//!   `item_state` trigger's `ON CONFLICT DO UPDATE` sets exactly the columns
//!   its insert sets, and the stray row holds the defaults in the others.
//!
//! Every full-table model takes these changes with `absorb_stray_rows`, as
//! the window it verifies allows; an insert model first clears its new
//! track's pid with `replace_stray_rows`. A pid with an `item` row on either
//! side is never stray: a track's own rows stay exact.
use super::{Tables, table};
use crate::catalogue::sqlite::tables::{Cell, Rows, Table};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The tables beside `item`, keyed by `item_pid`, that can hold rows whose
/// track is gone.
pub const STRAY_TABLES: [&str; 7] = [
    "item_extra",
    "item_store",
    "item_stats",
    "item_state",
    "item_search",
    "item_playback",
    "item_video",
];
/// The tables whose stray rows the sweep deletes.
const SWEPT: [&str; 2] = ["item_store", "item_stats"];
/// The table whose stray rows nothing deletes.
const KEPT: &str = "item_state";

/// When a capture was taken, relative to the session a model verifies: what
/// the phone may have done to stray rows meanwhile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrayWindow {
    /// The run's own capture after a session without file transfers (a
    /// removal, a playlist deletion, a metadata edit): nothing.
    WithoutTransfers,
    /// The run's own capture after a session with file transfers (an
    /// insert), or a capture moments after that one: stray `item_store` and
    /// `item_stats` rows may be gone (the sweep). Nothing else.
    WithTransfers,
    /// A later run's capture: any stray row but an `item_state` row may be
    /// gone (the sanitize), and any may have appeared or changed (a late
    /// write). A stray `item_state` row that is gone is left for the
    /// caller's comparison to refuse.
    BetweenRuns,
}
impl StrayWindow {
    fn may_vanish(self, name: &str) -> bool {
        match self {
            Self::WithoutTransfers => false,
            Self::WithTransfers => SWEPT.contains(&name),
            Self::BetweenRuns => name != KEPT,
        }
    }
    fn may_write(self) -> bool {
        self == Self::BetweenRuns
    }
}

/// What a model took of one table's stray rows, by `item_pid`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StrayChanges {
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub vanished: BTreeSet<i64>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub appeared: BTreeSet<i64>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub changed: BTreeSet<i64>,
    /// Rows a new track's import replaced (`replace_stray_rows`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub replaced: BTreeSet<i64>,
}

/// What a model took of rows whose track is gone, by table; tables with
/// nothing taken are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StrayRows(BTreeMap<&'static str, StrayChanges>);
impl StrayRows {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn table(&self, name: &str) -> Option<&StrayChanges> {
        self.0.get(name)
    }
    /// Adds what `other` took.
    pub fn extend(&mut self, other: StrayRows) {
        for (name, taken) in other.0 {
            let changes = self.entry(name);
            changes.vanished.extend(taken.vanished);
            changes.appeared.extend(taken.appeared);
            changes.changed.extend(taken.changed);
            changes.replaced.extend(taken.replaced);
        }
    }
    fn entry(&mut self, name: &'static str) -> &mut StrayChanges {
        self.0.entry(name).or_default()
    }
}

/// Take into `expected`, a model of a session applied to its before state,
/// each change to stray rows that `after` shows and `window` allows. A pid
/// is stray when neither `expected` nor `after` has an `item` row for it.
/// Any other difference, in these tables or elsewhere, is left for the
/// caller's exact comparison to refuse. Returns what was taken; on an error
/// `expected` is unchanged.
pub fn absorb_stray_rows(
    expected: &mut Tables,
    after: &Tables,
    window: StrayWindow,
) -> Result<StrayRows> {
    let mut live = item_pids(expected)?;
    live.extend(item_pids(after)?);
    let mut taken = StrayRows::default();
    let mut edits = Vec::new();
    for name in STRAY_TABLES {
        let (Some(model), Some(actual)) = (expected.get(name), after.get(name)) else {
            continue;
        };
        ensure!(
            model.columns == actual.columns,
            "{name} columns differ between the model and the after state"
        );
        let old = gone(model, &live)?;
        let new = gone(actual, &live)?;
        let pids: BTreeSet<i64> = old.keys().chain(new.keys()).copied().collect();
        for pid in pids {
            let (was, now) = (old.get(&pid), new.get(&pid));
            let kind: fn(&mut StrayChanges) -> &mut BTreeSet<i64> = match (was, now) {
                (Some(was), Some(now)) if was == now => continue,
                (Some(_), None) if window.may_vanish(name) => |c| &mut c.vanished,
                (None, Some(_)) if window.may_write() => |c| &mut c.appeared,
                (Some(_), Some(_)) if window.may_write() => |c| &mut c.changed,
                _ => continue,
            };
            kind(taken.entry(name)).insert(pid);
            edits.push((name, was.cloned(), now.cloned()));
        }
    }
    for (name, was, now) in edits {
        let Some(model) = expected.get_mut(name) else {
            continue;
        };
        for row in was.into_iter().flat_map(Rows::into_keys) {
            model.rows.remove(&row);
        }
        for (row, n) in now.into_iter().flatten() {
            *model.rows.entry(row).or_default() += n;
        }
    }
    Ok(taken)
}

/// Remove from `expected`, a model still at its before state, the stray
/// rows at `slot`, the pid a new track takes: its import replaces them and
/// takes over the `item_state` row, so the caller adds the new track's rows
/// in their place. Returns them as replaced. `slot` must have no `item` row.
pub fn replace_stray_rows(expected: &mut Tables, slot: i64) -> Result<StrayRows> {
    ensure!(
        !item_pids(expected)?.contains(&slot),
        "the new track's slot {slot} still has an item row"
    );
    let mut keys = Vec::new();
    for name in STRAY_TABLES {
        if let Some(t) = expected.get(name) {
            keys.push((name, t.column("item_pid")?));
        }
    }
    let mut taken = StrayRows::default();
    for (name, key) in keys {
        let Some(t) = expected.get_mut(name) else {
            continue;
        };
        let count = t.rows.len();
        t.rows.retain(|r, _| r[key] != Cell::Integer(slot));
        if t.rows.len() != count {
            taken.entry(name).replaced.insert(slot);
        }
    }
    Ok(taken)
}

/// The `item_pid` of the track whose `item_store` row holds `sync_id`. A
/// row whose track is gone keeps its sync id taken but names no track, so it
/// is passed over.
pub fn track_with_sync_id(tables: &Tables, sync_id: i64) -> Result<i64> {
    let items = item_pids(tables)?;
    let store = table(tables, "item_store")?;
    let (key, sync) = (store.column("item_pid")?, store.column("sync_id")?);
    let mut found = store.rows.iter().filter(|(r, _)| {
        r[sync] == Cell::Integer(sync_id)
            && matches!(r[key], Cell::Integer(pid) if items.contains(&pid))
    });
    let (row, n) = found.next().context("missing sync_id")?;
    ensure!(*n == 1 && found.next().is_none(), "nonunique sync_id");
    row[key].integer()
}

/// The `item_pid`s of `tables`' `item` rows.
fn item_pids(tables: &Tables) -> Result<BTreeSet<i64>> {
    let items = table(tables, "item")?;
    let key = items.column("item_pid")?;
    items.rows.keys().map(|r| r[key].integer()).collect()
}

/// `t`'s rows whose `item_pid` is not in `live`, by `item_pid`.
fn gone(t: &Table, live: &BTreeSet<i64>) -> Result<BTreeMap<i64, Rows>> {
    let key = t.column("item_pid")?;
    let mut found: BTreeMap<i64, Rows> = BTreeMap::new();
    for (row, n) in &t.rows {
        if let Cell::Integer(pid) = row[key]
            && !live.contains(&pid)
        {
            *found
                .entry(pid)
                .or_default()
                .entry(row.clone())
                .or_default() += n;
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
