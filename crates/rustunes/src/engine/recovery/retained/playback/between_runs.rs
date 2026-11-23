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
//! other table is left for the caller's exact comparison. Rows whose track is
//! gone are left to the stray rule (`absorb_stray_rows`, as
//! `Window::stray_rows` allows), which the caller applies first. It grants no
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
    /// The run's own post-write capture, compared exactly. Music may play
    /// while a run works, so a run's own check that this refuses is decided
    /// by `LaterRun` (`Window::check`).
    #[default]
    SameRun,
    /// A capture after the phone may have been used: a later run's, or the
    /// run's own when music played while it worked.
    LaterRun,
}
impl Window {
    pub(crate) fn is_same_run(&self) -> bool {
        *self == Self::SameRun
    }
    /// Runs `check` under this window and returns the window that passed. A
    /// run's own check (`SameRun`) takes the exact model first. Music may
    /// play while a run works, so when the exact model refuses, the later-run
    /// rule decides over the same captures: it takes listening and the
    /// phone's housekeeping and still pins everything the session wrote. Its
    /// refusal is the one returned; the exact one goes to the log. A later
    /// run's check is the later-run rule alone.
    pub(crate) fn check<T>(self, mut check: impl FnMut(Self) -> Result<T>) -> Result<(T, Self)> {
        if self == Self::LaterRun {
            return check(Self::LaterRun).map(|value| (value, Self::LaterRun));
        }
        let exact = match check(Self::SameRun) {
            Ok(value) => return Ok((value, Self::SameRun)),
            Err(error) => error,
        };
        tracing::info!(
            "The exact check refused ({exact:#}); checking again for music played during the run"
        );
        check(Self::LaterRun).map(|value| (value, Self::LaterRun))
    }
    /// What the phone may have done to rows whose track is gone, for a
    /// write whose session transferred files (`transfers`: an insert) or
    /// not. A later run's window takes it before `absorb`, whose statistics
    /// check counts the rows of tracks in the library only.
    pub(crate) fn stray_rows(self, transfers: bool) -> StrayWindow {
        match (self, transfers) {
            (Self::LaterRun, _) => StrayWindow::BetweenRuns,
            (Self::SameRun, true) => StrayWindow::WithTransfers,
            (Self::SameRun, false) => StrayWindow::WithoutTransfers,
        }
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
        &deleted_tracks(&table(observed, "entity_revision")?, budget)?,
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
            history.names_deleted,
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
/// entries by media type. Each existing row may move by exactly the change
/// in entries whose track is in the library. The phone does not lower them
/// when a removal leaves entries naming gone tracks, so they may count more
/// than the entries of tracks in the library. When the player appends an
/// entry for a track deleted while it played (`recount`), the phone counts
/// again (observed): each row may instead become the number of entries whose
/// track is in the library, and a row left with none goes. A row so gone
/// comes back with the next entries for tracks in the library, counting
/// them from none. Every other row stays exact, and any other row appearing
/// or going refuses.
#[allow(clippy::too_many_arguments)]
fn history_media_types(
    pid: i64,
    old: &Table,
    new: &Table,
    old_entries: &Table,
    new_entries: &Table,
    items: &Table,
    recount: bool,
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
    // Counted again with no entry left for a track of its type in the
    // library, a row goes (observed).
    ensure!(
        old_counts
            .keys()
            .all(|kind| { new_counts.contains_key(kind) || recount && !after.contains_key(kind) }),
        "Playback History media type rows removed"
    );
    let kinds: BTreeSet<i64> = old_counts
        .keys()
        .chain(new_counts.keys())
        .copied()
        .collect();
    let mut changes = Vec::new();
    for kind in kinds {
        let live = after.get(&kind).copied().unwrap_or(0);
        let delta = live - before.get(&kind).copied().unwrap_or(0);
        match (
            old_counts.get(&kind).copied(),
            new_counts.get(&kind).copied(),
        ) {
            (Some(was), None) => changes
                .push(json!({"media_type":kind,"before":was,"after":null,"counted_again":true})),
            // A history emptied by a clear counts again from none when the
            // next entry for a track in the library comes (observed).
            (None, Some(now)) => {
                ensure!(
                    delta > 0 && now == delta,
                    "Playback History media type {kind} row appeared with {now} for {delta} library entries"
                );
                changes.push(json!({"media_type":kind,"before":null,"after":now}));
            }
            (Some(was), Some(now)) => {
                let counted_again = recount && now == live;
                ensure!(
                    now - was == delta || counted_again,
                    "Playback History media type {kind} count moved by {} for {delta} library entries",
                    now - was
                );
                if now - was != delta {
                    changes.push(
                        json!({"media_type":kind,"before":was,"after":now,"counted_again":true}),
                    );
                } else if now != was {
                    changes.push(json!({"media_type":kind,"before":was,"after":now}));
                }
            }
            (None, None) => {}
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
/// been cleared by the write before the later events were counted. Only the
/// rows of tracks in the library (`items`) are counted; rows whose track is
/// gone are the stray rule's (`absorb_stray_rows`), which the caller applies
/// first, and must be equal here.
fn statistics(
    a: &Table,
    b: &Table,
    items: &BTreeMap<i64, Row>,
    budget: &Budget,
) -> Result<(BTreeMap<i64, Played>, Vec<Value>)> {
    let (old, old_gone) = by_track(a, items)?;
    let (new, new_gone) = by_track(b, items)?;
    ensure!(
        old.keys().eq(new.keys()),
        "playback statistics membership changed"
    );
    ensure!(
        old_gone == new_gone,
        "playback statistics of tracks that are gone changed outside the stray rule"
    );
    let mut events = BTreeMap::new();
    let mut resets = Vec::new();
    for (pid, before) in &old {
        budget.check()?;
        let after = &new[pid];
        if before == after {
            continue;
        }
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

/// `t`'s rows of tracks in `items`, by item, and its other rows.
fn by_track(
    t: &Table,
    items: &BTreeMap<i64, Row>,
) -> Result<(BTreeMap<i64, Row>, BTreeMap<Row, u32>)> {
    let (mut own, mut gone) = (BTreeMap::new(), BTreeMap::new());
    for (row, n) in &t.rows {
        let side = if items.contains_key(&num(t, row, "item_pid")?) {
            &mut own
        } else {
            &mut gone
        };
        side.insert(row.clone(), *n);
    }
    let own = Table {
        columns: t.columns.clone(),
        definitions: t.definitions.clone(),
        rows: own,
    };
    Ok((keyed(&own, "item_pid")?, gone))
}

#[cfg(test)]
mod tests;
