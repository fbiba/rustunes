//! Listening and phone housekeeping between a run's write and a later capture.
//!
//! `expected` is a route's exact model of its write applied to the retained
//! Before. Before a later `resume`, the phone may record plays and skips,
//! append to Playback History, renumber live items' full-record revisions,
//! drop superseded statistics revisions and advance its maintenance clock.
//! The write's own sync session may also have cleared recent counters, which
//! later listening counts up from zero. `absorb` proves that the observed
//! playback tables differ from `expected` only by those effects, using the
//! bridge's phone-observed rules, and then copies them into `expected`. Every
//! other table is left for the caller's exact comparison. It grants no
//! transport, replay or journal transition.
use super::*;

pub(crate) const TABLES: [&str; 5] = [
    "item_stats",
    "container",
    "container_item",
    "entity_revision",
    "_MLDatabaseProperties",
];

const FIELDS: [&str; 7] = [
    "play_count_user",
    "play_count_recent",
    "has_been_played",
    "date_played",
    "skip_count_user",
    "skip_count_recent",
    "date_skipped",
];

/// When the observed capture was taken, relative to the write it checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Window {
    /// The run's own post-write capture. The phone is not used during a run,
    /// so the model must match exactly.
    #[default]
    SameRun,
    /// A capture by a later `resume`, after the phone may have been used.
    LaterRun,
}
impl Window {
    pub(crate) fn is_same_run(&self) -> bool {
        *self == Self::SameRun
    }
    /// Absorbs listening and housekeeping into `expected` for a later-run
    /// capture only. Returns `None` when nothing was absorbed.
    pub(crate) fn absorb(
        self,
        expected: &mut BTreeMap<String, Table>,
        observed: &BTreeMap<String, Table>,
        budget: &Budget,
    ) -> Result<Option<Value>> {
        match self {
            Self::SameRun => Ok(None),
            Self::LaterRun => absorb(expected, observed, budget, None),
        }
    }
}

/// `absorb` for a model in which the session itself wrote revisions above
/// numbers it then renumbered (a removal's survivor refresh lies above its
/// tombstones, interleaved with moved album revisions): renumbered and fresh
/// revisions need only exceed `floor`, the highest tombstone, instead of the
/// model's highest revision. Every revision the model pins stays exact.
pub(crate) fn absorb_above(
    expected: &mut BTreeMap<String, Table>,
    observed: &BTreeMap<String, Table>,
    budget: &Budget,
    floor: i64,
) -> Result<Option<Value>> {
    absorb(expected, observed, budget, Some(floor))
}

/// Returns `None` when the playback tables already equal the model. On any
/// refusal `expected` is unchanged.
fn absorb(
    expected: &mut BTreeMap<String, Table>,
    observed: &BTreeMap<String, Table>,
    budget: &Budget,
    floor: Option<i64>,
) -> Result<Option<Value>> {
    budget.check()?;
    let table = |tables: &BTreeMap<String, Table>, name: &str| -> Result<Table> {
        tables
            .get(name)
            .cloned()
            .with_context(|| format!("missing playback table {name}"))
    };
    let mut changed = false;
    for name in TABLES {
        let (old, new) = (table(expected, name)?, table(observed, name)?);
        ensure!(
            old.columns == new.columns && old.definitions == new.definitions,
            "playback table definition differs: {name}"
        );
        changed |= old.rows != new.rows;
    }
    if !changed {
        return Ok(None);
    }
    let items = keyed(&table(expected, "item")?, "item_pid")?;
    let (events, resets) = statistics(
        &table(expected, "item_stats")?,
        &table(observed, "item_stats")?,
        &items,
        budget,
    )?;
    let history = check_history(
        &table(expected, "container")?,
        &table(observed, "container")?,
        &table(expected, "container_item")?,
        &table(observed, "container_item")?,
        &items,
        budget,
    )?;
    let revisions = check_revisions(
        floor,
        &table(expected, "entity_revision")?,
        &table(observed, "entity_revision")?,
        &events,
        &items,
        history.changed_container,
        budget,
    )?;
    let maintenance = ordinary_maintenance::check(
        &table(expected, "_MLDatabaseProperties")?,
        &table(observed, "_MLDatabaseProperties")?,
        budget,
    )?;
    let media_types = match (
        history.changed_container,
        expected.get(MEDIA_TYPES),
        observed.get(MEDIA_TYPES),
    ) {
        (Some(pid), Some(old), Some(new)) if old.rows != new.rows => Some(history_media_types(
            pid,
            old,
            new,
            &table(expected, "container_item")?,
            &table(observed, "container_item")?,
            &table(expected, "item")?,
            budget,
        )?),
        _ => None,
    };
    budget.check()?;
    for name in TABLES {
        expected.insert(name.into(), table(observed, name)?);
    }
    if media_types.is_some() {
        expected.insert(MEDIA_TYPES.into(), table(observed, MEDIA_TYPES)?);
    }
    Ok(Some(json!({
        "schema":"rustunes.between-runs-playback",
        "completed_events":events.values().collect::<Vec<_>>(),
        "recent_counter_resets":resets,"playback_history":history.report,
        "entity_revisions":revisions,"maintenance_task_operation_date":maintenance,
        "history_media_type_counts":media_types,
        "content_tables_compared_by_caller":true,
    })))
}

const MEDIA_TYPES: &str = "container_item_media_type";

/// The Playback History's rows in `container_item_media_type` count its
/// entries whose track is in the library, by media type (entries naming
/// tracks removed earlier are not counted). Each existing row may move by
/// exactly the change in such entries; every other row stays exact, and a
/// row appearing or going refuses.
fn history_media_types(
    pid: i64,
    old: &Table,
    new: &Table,
    old_entries: &Table,
    new_entries: &Table,
    items: &Table,
    budget: &Budget,
) -> Result<Value> {
    ensure!(
        old.columns == new.columns && old.definitions == new.definitions,
        "playback table definition differs: {MEDIA_TYPES}"
    );
    let kinds = {
        let mut kinds = BTreeMap::new();
        for row in items.rows.keys() {
            kinds.insert(num(items, row, "item_pid")?, num(items, row, "media_type")?);
        }
        kinds
    };
    let live = |t: &Table| -> Result<BTreeMap<i64, i64>> {
        let mut counts = BTreeMap::new();
        for (row, n) in &t.rows {
            budget.check()?;
            if num(t, row, "container_pid")? != pid {
                continue;
            }
            if let Some(kind) = kinds.get(&num(t, row, "item_pid")?) {
                *counts.entry(*kind).or_default() += i64::from(*n);
            }
        }
        Ok(counts)
    };
    let (before, after) = (live(old_entries)?, live(new_entries)?);
    let rows = |t: &Table| -> Result<(BTreeMap<i64, i64>, BTreeMap<Row, u32>)> {
        let mut own = BTreeMap::new();
        let mut other = BTreeMap::new();
        for (row, n) in &t.rows {
            if num(t, row, "container_pid")? == pid {
                let previous = own.insert(num(t, row, "media_type")?, num(t, row, "count")?);
                ensure!(
                    *n == 1 && previous.is_none(),
                    "ambiguous Playback History media type count"
                );
            } else {
                other.insert(row.clone(), *n);
            }
        }
        Ok((own, other))
    };
    let (old_counts, old_other) = rows(old)?;
    let (new_counts, new_other) = rows(new)?;
    ensure!(
        old_other == new_other,
        "unrelated playlist media type count changed"
    );
    ensure!(
        old_counts.keys().eq(new_counts.keys()),
        "Playback History media type rows added or removed"
    );
    let mut changes = Vec::new();
    for (kind, was) in &old_counts {
        let now = new_counts[kind];
        let delta = after.get(kind).copied().unwrap_or(0) - before.get(kind).copied().unwrap_or(0);
        ensure!(
            now - was == delta,
            "Playback History media type {kind} count moved by {} for {delta} library entries",
            now - was
        );
        if now != *was {
            changes.push(json!({"media_type":kind,"before":was,"after":now}));
        }
    }
    Ok(json!(changes))
}

/// Whether an absorption report records more than recent counters the write
/// itself cleared: plays, skips, history, revisions or the maintenance clock.
pub(crate) fn listened(report: &Value) -> bool {
    ["completed_events", "entity_revisions"]
        .iter()
        .any(|key| report[key].as_array().is_some_and(|v| !v.is_empty()))
        || !report["playback_history"].is_null()
        || !report["maintenance_task_operation_date"].is_null()
}

/// The catalogue projection of the same rule, for checks that compare tracks
/// rather than SQL rows: lifetime counts only grow, each recent counter keeps
/// its value or restarts from zero plus the later events, `has_been_played`
/// follows plays, and rating and like state stay exact.
pub(crate) fn catalogue_listening(
    before: &music_protocol::catalogue::sqlite::PlaybackStatistics,
    after: &music_protocol::catalogue::sqlite::PlaybackStatistics,
) -> Result<()> {
    ensure!(
        (
            before.user_rating,
            before.user_rating_is_derived,
            before.liked_state
        ) == (
            after.user_rating,
            after.user_rating_is_derived,
            after.liked_state
        ),
        "rating or like state changed between runs"
    );
    let plays = after
        .play_count_user
        .checked_sub(before.play_count_user)
        .context("lifetime play count decreased")?;
    let skips = after
        .skip_count_user
        .checked_sub(before.skip_count_user)
        .context("lifetime skip count decreased")?;
    for (field, prior, now, events) in [
        (
            "play_count_recent",
            before.play_count_recent,
            after.play_count_recent,
            plays,
        ),
        (
            "skip_count_recent",
            before.skip_count_recent,
            after.skip_count_recent,
            skips,
        ),
    ] {
        ensure!(
            prior.checked_add(events) == Some(now) || (prior > 0 && now == events),
            "{field} is neither kept nor cleared before the later events"
        );
    }
    ensure!(
        after.has_been_played == (before.has_been_played || plays > 0),
        "has_been_played is not explained by completed plays"
    );
    Ok(())
}

/// The ordinary play/skip rule, except that a positive recent counter may have
/// been cleared by the write before the later events were counted.
fn statistics(
    a: &Table,
    b: &Table,
    items: &BTreeMap<i64, Row>,
    budget: &Budget,
) -> Result<(BTreeMap<i64, Played>, Vec<Value>)> {
    let old = keyed(a, "item_pid")?;
    let new = keyed(b, "item_pid")?;
    ensure!(
        old.keys().eq(new.keys()),
        "playback statistics membership changed"
    );
    let mut events = BTreeMap::new();
    let mut resets = Vec::new();
    for (pid, before) in &old {
        budget.check()?;
        let after = &new[pid];
        if before == after {
            continue;
        }
        ensure!(
            items.contains_key(pid),
            "playback statistics reference a foreign item"
        );
        exact_except(a, before, after, &FIELDS)?;
        let count = |t: &Table, r: &Row, field: &str| -> Result<i64> {
            let n = num(t, r, field)?;
            ensure!(
                (0..=i64::from(u32::MAX)).contains(&n),
                "playback counter is negative or exceeds native range: {field}"
            );
            Ok(n)
        };
        let delta = |field: &str| -> Result<i64> {
            let d = count(b, after, field)? - count(a, before, field)?;
            ensure!(d >= 0, "lifetime playback counter decreased: {field}");
            Ok(d)
        };
        let plays = delta("play_count_user")?;
        let skips = delta("skip_count_user")?;
        let mut cleared = Vec::new();
        for (field, events) in [("play_count_recent", plays), ("skip_count_recent", skips)] {
            let prior = count(a, before, field)?;
            let now = count(b, after, field)?;
            if now != prior + events {
                ensure!(
                    prior > 0 && now == events,
                    "{field} is neither kept nor cleared before the later events"
                );
                cleared.push(field);
            }
        }
        let old_played = num(a, before, "has_been_played")?;
        ensure!(
            (0..=1).contains(&old_played)
                && num(b, after, "has_been_played")? == if plays > 0 { 1 } else { old_played },
            "has_been_played is not explained by completed plays"
        );
        for (field, count) in [("date_played", plays), ("date_skipped", skips)] {
            let (old_date, new_date) = (a.at(before, field)?, b.at(after, field)?);
            if count == 0 {
                ensure!(old_date == new_date, "{field} changed without its event");
            } else {
                ensure!(
                    number(new_date)? > number(old_date)?,
                    "{field} did not advance"
                );
            }
        }
        let fields = |t: &Table, r: &Row, keys: &[&str]| -> Result<BTreeMap<String, Cell>> {
            keys.iter()
                .map(|key| Ok(((*key).into(), t.at(r, key)?.clone())))
                .collect()
        };
        if !cleared.is_empty() {
            let recent = |t: &Table, r: &Row| -> Result<Value> {
                Ok(
                    json!({"play_count_recent":count(t, r, "play_count_recent")?,
                    "skip_count_recent":count(t, r, "skip_count_recent")?}),
                )
            };
            resets.push(json!({"item_pid":pid,"cleared":cleared,
                "before":recent(a, before)?,"after":recent(b, after)?}));
        }
        if plays > 0 || skips > 0 {
            events.insert(
                *pid,
                Played {
                    item_pid: *pid,
                    play_count_delta: u64::try_from(plays)?,
                    skip_count_delta: u64::try_from(skips)?,
                    before: fields(a, before, &FIELDS)?,
                    after: fields(b, after, &FIELDS)?,
                },
            );
        }
    }
    Ok((events, resets))
}

#[cfg(test)]
mod tests;
