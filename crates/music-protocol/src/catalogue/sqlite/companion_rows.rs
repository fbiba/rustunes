//! The four tables beside `item` that hold one row per track, counted
//! against `item` (docs/04, section 4.1).
//!
//! The phone can hold rows in them whose track is gone: a property write
//! for a deleted track inserts a row in the table it writes, and the
//! `item_state_*_insert_sync` triggers add an `item_state` row. The phone
//! removes some of these later and never the `item_state` row, and its own
//! consistency check does not count `item_state`. The reader passes over
//! them. A track without a row in one of these tables has no known ordinary
//! cause, and the catalogue refuses it.
use super::{Connection, ReadControl, Result, check_control};
use anyhow::{Context, bail};
use rusqlite::OptionalExtension;

/// The tables beside `item` with one row per track, keyed by `item_pid`.
pub const COMPANION_TABLES: [&str; 4] = ["item_extra", "item_store", "item_stats", "item_state"];

/// One companion table's rows, counted against `item`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompanionRows {
    pub table: &'static str,
    pub rows: u64,
    /// `item` rows with no row in this table.
    pub missing: u64,
    /// Rows whose `item_pid` has no `item` row: left by a write for a track
    /// that was deleted.
    pub stray: u64,
}

/// `item`'s rows and each companion table's, as the database holds them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemRowCounts {
    pub items: u64,
    /// In `COMPANION_TABLES` order.
    pub companions: [CompanionRows; 4],
    /// The gone tracks stray rows are left for: their distinct `item_pid`s
    /// across the four tables, with the lowest and the highest.
    pub stray_tracks: u64,
    pub stray_pids: Option<(i64, i64)>,
    /// The highest `item_pid` of `item`, when it holds a row.
    pub highest_item_pid: Option<i64>,
}

/// The most new tracks `ItemRowCounts::songs_until_cleared` counts on: more
/// would mean the phone does not number its tracks one by one.
pub const SONGS_UNTIL_CLEARED_LIMIT: u64 = 1000;

impl ItemRowCounts {
    pub(super) fn read(connection: &Connection, control: Option<&ReadControl>) -> Result<Self> {
        let count = |sql: &str| -> Result<u64> {
            check_control(control)?;
            let value: i64 = connection.query_row(sql, [], |r| r.get(0))?;
            u64::try_from(value).context("negative row count")
        };
        let items = count("SELECT count(*) FROM item")?;
        let mut companions = COMPANION_TABLES.map(|table| CompanionRows {
            table,
            rows: 0,
            missing: 0,
            stray: 0,
        });
        for companion in &mut companions {
            let table = companion.table;
            companion.rows = count(&format!("SELECT count(*) FROM {table}"))?;
            companion.missing = count(&format!(
                "SELECT count(*) FROM item i LEFT JOIN {table} t USING(item_pid) \
                 WHERE t.item_pid IS NULL"
            ))?;
            companion.stray = count(&format!(
                "SELECT count(*) FROM {table} t LEFT JOIN item i USING(item_pid) \
                 WHERE i.item_pid IS NULL"
            ))?;
        }
        // The reader's connection allows no function but count, so the
        // extremes are read by order.
        const GONE: &str = "FROM (SELECT item_pid FROM item_extra UNION SELECT item_pid FROM item_store \
             UNION SELECT item_pid FROM item_stats UNION SELECT item_pid FROM item_state) s \
             LEFT JOIN item i USING(item_pid) WHERE i.item_pid IS NULL";
        let stray_tracks = count(&format!("SELECT count(*) {GONE}"))?;
        let pid = |sql: &str| -> Result<Option<i64>> {
            check_control(control)?;
            Ok(connection.query_row(sql, [], |r| r.get(0)).optional()?)
        };
        let lowest = pid(&format!(
            "SELECT s.item_pid {GONE} ORDER BY s.item_pid LIMIT 1"
        ))?;
        let highest = pid(&format!(
            "SELECT s.item_pid {GONE} ORDER BY s.item_pid DESC LIMIT 1"
        ))?;
        let highest_item_pid = pid("SELECT item_pid FROM item ORDER BY item_pid DESC LIMIT 1")?;
        Ok(Self {
            items,
            companions,
            stray_tracks,
            stray_pids: lowest.zip(highest),
            highest_item_pid,
        })
    }

    /// About how many new tracks clear every stray row: none are left once
    /// a new track has taken each stray pid, as the phone numbers a new track
    /// one above the highest `item_pid` and a track given a stray row's pid
    /// takes the row over (docs/04 §4.6). None without stray rows, when some
    /// stray pid is not above every track's, which new tracks never reach,
    /// and when more than `SONGS_UNTIL_CLEARED_LIMIT` would be needed.
    pub fn songs_until_cleared(&self) -> Option<u64> {
        let (lowest, highest) = self.stray_pids?;
        let live = self.highest_item_pid?;
        if lowest <= live {
            return None;
        }
        u64::try_from(i128::from(highest) - i128::from(live))
            .ok()
            .filter(|n| *n <= SONGS_UNTIL_CLEARED_LIMIT)
    }

    /// The tables holding rows whose track is gone, with how many.
    pub fn stray(&self) -> impl Iterator<Item = &CompanionRows> {
        self.companions.iter().filter(|c| c.stray != 0)
    }

    /// The tables some track has no row in, with how many.
    pub fn missing(&self) -> impl Iterator<Item = &CompanionRows> {
        self.companions.iter().filter(|c| c.missing != 0)
    }

    /// `item_extra 1, item_state 1`: each table holding rows whose track is
    /// gone. None when no table does.
    pub fn stray_summary(&self) -> Option<String> {
        let parts: Vec<_> = self
            .stray()
            .map(|c| format!("{} {}", c.table, c.stray))
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// What is inconsistent when some track lacks a companion row, with
    /// every table's count: `3 of 12 tracks have no item_state row (item 12,
    /// item_extra 12, item_store 12, item_stats 12, item_state 9; stray rows:
    /// none)`. None when every track has its rows.
    pub fn inconsistency(&self) -> Option<String> {
        let missing: Vec<_> = self
            .missing()
            .map(|c| {
                let (noun, verb) = if c.missing == 1 {
                    ("track", "has")
                } else {
                    ("tracks", "have")
                };
                format!(
                    "{} of {} {noun} {verb} no {} row",
                    c.missing, self.items, c.table
                )
            })
            .collect();
        if missing.is_empty() {
            return None;
        }
        let counts: Vec<_> = self
            .companions
            .iter()
            .map(|c| format!("{} {}", c.table, c.rows))
            .collect();
        Some(format!(
            "{} (item {}, {}; stray rows: {})",
            missing.join(", "),
            self.items,
            counts.join(", "),
            self.stray_summary().as_deref().unwrap_or("none")
        ))
    }

    /// Fail when some track lacks a companion row, naming the tables and
    /// the counts.
    pub fn ensure_complete(&self) -> Result<()> {
        match self.inconsistency() {
            Some(inconsistency) => {
                bail!("the iPhone's music library is inconsistent: {inconsistency}")
            }
            None => Ok(()),
        }
    }
}
