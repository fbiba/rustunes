//! Survivors' recent play/skip counters across CLI runs.
//!
//! Music is played on the phone between runs, so a fresh Before routinely
//! holds positive recent counters, and a native removal may clear them. Each
//! survivor counter may keep its Before value or fall from positive to zero.
//! Every other statistics cell, row identity and multiplicity stays exact: the
//! caller's whole-table comparison still decides.
use crate::engine::verification::sql_tables::reader::{Budget, Cell, Row, Table};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const PLAY: &str = "play_count_recent";
const SKIP: &str = "skip_count_recent";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Reset {
    pub(crate) item_pid: i64,
    pub(crate) previous_play: i64,
    pub(crate) previous_skip: i64,
    pub(crate) play: i64,
    pub(crate) skip: i64,
}

/// Before qualification: one statistics row per item and non-negative recent
/// counters. Positive counters are ordinary listening, not drift.
pub(crate) fn qualify(stats: &Table) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (row, multiplicity) in &stats.rows {
        let pid = stats.at(row, "item_pid")?.integer()?;
        ensure!(
            *multiplicity == 1 && seen.insert(pid),
            "duplicate item statistics"
        );
        for field in [PLAY, SKIP] {
            ensure!(
                stats.at(row, field)?.integer()? >= 0,
                "negative recent counter"
            );
        }
    }
    Ok(())
}

/// Rewrites each `expected` row whose observed counterpart differs only by
/// recent counters reset from positive to zero. Any other difference is left
/// in place for the caller's exact comparison to refuse.
pub(crate) fn absorb(
    expected: &mut Table,
    observed: &Table,
    budget: &Budget,
) -> Result<Vec<Reset>> {
    ensure!(
        expected.columns == observed.columns,
        "statistics schema differs"
    );
    let key = expected.column("item_pid")?;
    let play = expected.column(PLAY)?;
    let skip = expected.column(SKIP)?;
    let mut by_pid: BTreeMap<&Cell, Option<&Row>> = BTreeMap::new();
    for (row, multiplicity) in &observed.rows {
        budget.check()?;
        let unique = (*multiplicity == 1).then_some(row);
        by_pid
            .entry(&row[key])
            .and_modify(|seen| *seen = None)
            .or_insert(unique);
    }
    let mut approved = Vec::new();
    for (row, multiplicity) in &expected.rows {
        budget.check()?;
        let Some(Some(seen)) = by_pid.get(&row[key]) else {
            continue;
        };
        if *multiplicity != 1 || *seen == row {
            continue;
        }
        let mut reset = row.clone();
        for index in [play, skip] {
            if let (Cell::Integer(from), Cell::Integer(0)) = (&row[index], &seen[index])
                && *from > 0
            {
                reset[index] = Cell::Integer(0);
            }
        }
        if reset == **seen {
            approved.push((row.clone(), reset));
        }
    }
    let mut resets = Vec::new();
    for (old, new) in approved {
        resets.push(Reset {
            item_pid: old[key].integer()?,
            previous_play: old[play].integer()?,
            previous_skip: old[skip].integer()?,
            play: new[play].integer()?,
            skip: new[skip].integer()?,
        });
        expected.rows.remove(&old);
        *expected.rows.entry(new).or_default() += 1;
    }
    Ok(resets)
}

#[cfg(test)]
mod tests;
