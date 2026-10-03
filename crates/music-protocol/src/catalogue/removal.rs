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
pub(crate) struct Representative {
    table: &'static str,
    group_pid: i64,
    before: i64,
    after: i64,
}
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
/// A group row the session deleted because no item belongs to it any more.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CollectedGroup {
    table: &'static str,
    #[serde(skip)]
    key: &'static str,
    #[serde(skip)]
    class: i64,
    group_pid: i64,
    tombstone: Option<(i64, i64)>,
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
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RemovalReceipt {
    schema: &'static str,
    targets: Vec<TargetRemoval>,
    property_changes: BTreeMap<&'static str, (String, String)>,
    representatives: Vec<Representative>,
    recent_counter_resets: Vec<i64>,
    /// Entries the phone added itself whose state the session filled in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filled_phone_entries: Vec<i64>,
    removed_orphan_artwork_rows: u32,
    survivor_revision_refresh: Vec<RevisionRefresh>,
    emptied_albums: Vec<EmptiedAlbum>,
    /// Albums an earlier session left unevaluated that this one evaluated.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    evaluated_albums: Vec<EmptiedAlbum>,
    collected_groups: Vec<CollectedGroup>,
    playlists: Vec<playlists::PlaylistRemoval>,
    keep_local_evaluations: Vec<playlists::KeepLocalEvaluation>,
    /// What a later run's capture showed of the phone's use since the
    /// session (`verify_later`).
    #[serde(skip_serializing_if = "Option::is_none")]
    between_runs_playback: Option<serde_json::Value>,
}

impl RemovalReceipt {
    pub fn removed(&self) -> usize {
        self.targets.len()
    }
}

/// Albums still at keep_local_status 0 that the after state shows at 2 and
/// that the removal neither emptied nor collected: the phone finishing an
/// earlier session's evaluation. The caller holds them to the emptied-album
/// rule, revision move included.
fn unevaluated_albums(
    before: &Table,
    after: &Table,
    emptied: &BTreeSet<i64>,
    collected: &[CollectedGroup],
) -> Result<BTreeSet<i64>> {
    let mut found = BTreeSet::new();
    // A table without the column (a reduced fixture) has nothing to evaluate.
    if before.column("keep_local_status").is_err() {
        return Ok(found);
    }
    for row in before.rows.keys() {
        let id = typed::int(before, row, "album_pid")?;
        if emptied.contains(&id)
            || collected
                .iter()
                .any(|g| g.table == "album" && g.group_pid == id)
            || typed::int(before, row, "keep_local_status")? != 0
        {
            continue;
        }
        let Ok(now) = after.one("album_pid", id) else {
            continue;
        };
        if typed::int(after, now, "keep_local_status")? == 2 {
            found.insert(id);
        }
    }
    Ok(found)
}

/// Recent play/skip counters of tracks outside `removed` may reset in any
/// session, but only both to zero together; `expected` takes each such reset
/// the after state shows.
fn recent_counter_resets(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
    removed: &BTreeSet<i64>,
) -> Result<Vec<i64>> {
    let mut resets = Vec::new();
    let stats = table(before, "item_stats")?;
    let after_stats = table(after, "item_stats")?;
    for row in stats.rows.keys() {
        let id = typed::int(stats, row, "item_pid")?;
        if removed.contains(&id) {
            continue;
        }
        let mut reset = row.clone();
        typed::set(stats, &mut reset, "play_count_recent", Cell::Integer(0))?;
        typed::set(stats, &mut reset, "skip_count_recent", Cell::Integer(0))?;
        if reset != *row && after_stats.one("item_pid", id).ok() == Some(&reset) {
            typed::replace(expected, "item_stats", row, reset)?;
            resets.push(id);
        }
    }
    Ok(resets)
}

/// Columns the phone's sync code writes into an `item_state` row, with the
/// value it writes; each column's schema default is 0 and no trigger sets
/// it. Every track a sync made carries them.
const FILLED_STATE: [(&str, Cell); 3] = [
    ("contained_persistent_id", Cell::Null),
    ("lyrics_adam_id", Cell::Null),
    ("is_valid_content_type", Cell::Integer(1)),
];

/// The `item_state` rows of entries the phone added itself (`item_store`
/// sync ID 0) that the session filled in: every column of [`FILLED_STATE`]
/// still at 0 takes its value, all together, and no other cell changes.
/// Observed once: the first session after the phone added a streamed entry
/// (a removal) filled that entry's row so. Any session may do it, so the
/// playlist deletion and metadata edit models take it too. `expected` takes
/// each such row the after state shows; returns their items.
pub fn fill_phone_entries(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
) -> Result<Vec<i64>> {
    let mut filled_items = Vec::new();
    let (Some(store), Some(states)) = (before.get("item_store"), before.get("item_state")) else {
        return Ok(filled_items);
    };
    // A table without the columns (a reduced fixture) has nothing to fill.
    if FILLED_STATE
        .iter()
        .any(|(column, _)| states.column(column).is_err())
    {
        return Ok(filled_items);
    }
    let after_states = table(after, "item_state")?;
    for row in store.rows.keys() {
        if typed::int(store, row, "sync_id")? != 0 {
            continue;
        }
        let id = typed::int(store, row, "item_pid")?;
        let Ok(state) = states.one("item_pid", id) else {
            continue;
        };
        let mut filled = state.clone();
        for (column, value) in FILLED_STATE {
            if *states.at(state, column)? == Cell::Integer(0) {
                typed::set(states, &mut filled, column, value)?;
            }
        }
        if filled != *state
            && after_states.one("item_pid", id).ok() == Some(&filled)
            && table(expected, "item_state")?.rows.contains_key(state)
        {
            typed::replace(expected, "item_state", state, filled)?;
            filled_items.push(id);
        }
    }
    Ok(filled_items)
}

/// The sync properties' new values, taken into `expected` whatever they are;
/// the caller checks anchor, last synced and hero progress. The generation
/// must stay a signed decimal. The phone's own maintenance clock may advance
/// meanwhile; it carries no library content. Adding or removing it still
/// refuses in the caller's final comparison.
fn property_changes(
    before: &Tables,
    after: &Tables,
    expected: &mut Tables,
) -> Result<BTreeMap<&'static str, (String, String)>> {
    decimal(property(after, GENERATION)?, true)?;
    let mut changes = BTreeMap::new();
    let properties = table(before, "_MLDatabaseProperties")?;
    for key in [ANCHOR, LAST_SYNCED, GENERATION, HERO] {
        let old = property_row(before, key)?;
        let value = property(after, key)?;
        let previous = properties.at(old, "value")?.text()?;
        if value != previous {
            changes.insert(key, (previous.to_owned(), value.to_owned()));
            let mut desired = old.clone();
            typed::set(properties, &mut desired, "value", Cell::Text(value.into()))?;
            typed::replace(expected, "_MLDatabaseProperties", old, desired)?;
        }
    }
    if let (Ok(old), Ok(value)) = (
        property_row(before, MAINTENANCE),
        property(after, MAINTENANCE),
    ) {
        let previous = properties.at(old, "value")?.text()?;
        if value != previous {
            ensure!(
                maintenance_date(value)? > maintenance_date(previous)?,
                "removal maintenance date did not advance"
            );
            changes.insert(MAINTENANCE, (previous.to_owned(), value.to_owned()));
            let mut desired = old.clone();
            typed::set(properties, &mut desired, "value", Cell::Text(value.into()))?;
            typed::replace(expected, "_MLDatabaseProperties", old, desired)?;
        }
    }
    Ok(changes)
}

/// Rows of an artwork token table whose entity does not exist.
fn orphan_tokens(tables: &Tables, name: &str) -> Result<Vec<(Row, u32)>> {
    let tokens = table(tables, name)?;
    let mut orphans = Vec::new();
    for (row, n) in &tokens.rows {
        let kind = typed::int(tokens, row, "entity_type")?;
        let Some((_, entities, key)) = TOKEN_ENTITIES.iter().find(|e| e.0 == kind) else {
            continue;
        };
        let entities = table(tables, entities)?;
        let column = entities.column(key)?;
        let id = Cell::Integer(typed::int(tokens, row, "entity_pid")?);
        if !entities.rows.keys().any(|r| r[column] == id) {
            orphans.push((row.clone(), *n));
        }
    }
    Ok(orphans)
}

/// Group rows present before and absent after. Each must have no surviving
/// member; any other deleted group refuses.
fn collected_groups(
    before: &Tables,
    after: &Tables,
    survivors: &BTreeSet<i64>,
) -> Result<Vec<CollectedGroup>> {
    let items = table(before, "item")?;
    let mut collected = Vec::new();
    for (name, key, class) in GROUPS {
        let mut kept = BTreeSet::new();
        for row in items.rows.keys() {
            if survivors.contains(&typed::int(items, row, "item_pid")?) {
                kept.insert(typed::int(items, row, key)?);
            }
        }
        let groups = table(before, name)?;
        let after_groups = table(after, name)?;
        let column = after_groups.column(key)?;
        for row in groups.rows.keys() {
            let id = typed::int(groups, row, key)?;
            if after_groups
                .rows
                .keys()
                .any(|r| r[column] == Cell::Integer(id))
            {
                continue;
            }
            ensure!(!kept.contains(&id), "removal deleted {name} {id}");
            collected.push(CollectedGroup {
                table: name,
                key,
                class,
                group_pid: id,
                tombstone: None,
                removed_stats_revision: None,
            });
        }
    }
    Ok(collected)
}

/// True when the after state still holds every target exactly as before and
/// the anchor did not move: the session demonstrably did not apply. Anything
/// else needs the full model above.
pub fn not_applied(before: &Tables, after: &Tables, targets: &[i64]) -> Result<bool> {
    not_applied_with(before, after, targets, false)
}

/// `not_applied` for a later run's capture: a target the phone kept may have
/// been played or skipped since, so its statistics may differ in the
/// playback columns (lifetime counts only growing), and the phone may have
/// renumbered its live revision upwards. Every other cell is compared as
/// before.
pub fn not_applied_later(before: &Tables, after: &Tables, targets: &[i64]) -> Result<bool> {
    not_applied_with(before, after, targets, true)
}

fn not_applied_with(before: &Tables, after: &Tables, targets: &[i64], later: bool) -> Result<bool> {
    if property(before, ANCHOR)? != property(after, ANCHOR)? {
        return Ok(false);
    }
    // The cells a later run sets aside, by table.
    let aside = |name: &str| -> &[&str] {
        match (later, name) {
            (true, "item_stats") => &PLAYBACK_COLUMNS,
            _ => &[],
        }
    };
    for item in targets {
        for name in ITEM_TABLES {
            let b = table(before, name)?;
            let a = table(after, name)?;
            let key = b.column("item_pid")?;
            let own = |t: &Table| -> Result<Rows> {
                let mut rows = Rows::new();
                for (r, n) in t
                    .rows
                    .iter()
                    .filter(|(r, _)| r[key] == Cell::Integer(*item))
                {
                    let mut row = r.clone();
                    for column in aside(name) {
                        row[t.column(column)?] = Cell::Null;
                    }
                    *rows.entry(row).or_default() += *n;
                }
                Ok(rows)
            };
            if own(b)? != own(a)? || own(b)?.is_empty() && name == "item" {
                return Ok(false);
            }
        }
        if later {
            let stats = table(before, "item_stats")?;
            let now = table(after, "item_stats")?;
            if let (Ok(old), Ok(new)) = (stats.one("item_pid", *item), now.one("item_pid", *item)) {
                for count in ["play_count_user", "skip_count_user"] {
                    if typed::int(now, new, count)? < typed::int(stats, old, count)? {
                        return Ok(false);
                    }
                }
            }
        }
        let (old, old_number) = target_revision(before, *item)?;
        let (new, new_number) = target_revision(after, *item)?;
        let renumbered = || -> Result<bool> {
            let revisions = table(before, "entity_revision")?;
            let mut moved = old.clone();
            typed::set(revisions, &mut moved, "revision", Cell::Integer(new_number))?;
            Ok(new_number > old_number && moved == *new)
        };
        if old != new && !(later && renumbered()?) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The library container (`db_info.primary_container_pid`) and every
/// Playback History container keep a removed track's membership rows.
/// Synthetic tables without these two tables retain nothing.
fn retained_containers(before: &Tables) -> Result<BTreeSet<i64>> {
    let mut retained = BTreeSet::new();
    if let Some(info) = before.get("db_info") {
        for row in info.rows.keys() {
            retained.insert(typed::int(info, row, "primary_container_pid")?);
        }
    }
    if let Some(containers) = before.get("container") {
        for row in containers.rows.keys() {
            if typed::int(containers, row, "distinguished_kind")? == PLAYBACK_HISTORY {
                retained.insert(typed::int(containers, row, "container_pid")?);
            }
        }
    }
    retained.remove(&0);
    Ok(retained)
}

/// Optional receiver refresh of every survivor's live revision, and of each
/// emptied or late-evaluated album's revision (`albums_recomputed`), together
/// contiguous immediately above the highest tombstone. Such an album whose
/// revision moves may lose its stats revision row. Exact equality is the
/// ordinary path.
#[allow(clippy::type_complexity)]
fn survivor_revision_refresh(
    expected: &mut Table,
    actual: &Table,
    survivors: &BTreeSet<i64>,
    albums_recomputed: &BTreeSet<i64>,
    tombstone: i64,
    later: bool,
) -> Result<(Vec<RevisionRefresh>, BTreeMap<i64, (i64, i64, Option<i64>)>)> {
    if expected.rows == actual.rows {
        return Ok((Vec::new(), BTreeMap::new()));
    }
    let live = |t: &Table| -> Result<BTreeMap<i64, Row>> {
        let mut result = BTreeMap::new();
        for (row, n) in &t.rows {
            if typed::int(t, row, "deleted")? == 0
                && typed::int(t, row, "class")? == 0
                && typed::int(t, row, "revision_type")? == 0
            {
                let id = typed::int(t, row, "entity_pid")?;
                ensure!(
                    *n == 1 && survivors.contains(&id) && result.insert(id, row.clone()).is_none(),
                    "revision refresh has a foreign or duplicate live item"
                );
            }
        }
        ensure!(
            result.keys().eq(survivors.iter()),
            "revision refresh live items differ from survivors"
        );
        Ok(result)
    };
    for row in expected.rows.keys() {
        let revision = typed::int(expected, row, "revision")?;
        ensure!(
            revision > 0 && revision <= tombstone,
            "revision refresh baseline exceeds the tombstone"
        );
    }
    // An emptied or late-evaluated album's live revision may advance, and its
    // stats revision row go with it; nothing else in it.
    let column = expected.column("revision")?;
    let mut numbers = BTreeSet::new();
    let mut albums = BTreeMap::new();
    let album_row = |t: &Table, id: i64, kind: i64| -> Result<Option<Row>> {
        let mut found = None;
        for (row, n) in &t.rows {
            if typed::int(t, row, "entity_pid")? == id
                && typed::int(t, row, "class")? == ALBUM_CLASS
                && typed::int(t, row, "revision_type")? == kind
            {
                ensure!(*n == 1 && found.is_none(), "duplicate album revision");
                found = Some(row.clone());
            }
        }
        Ok(found)
    };
    for id in albums_recomputed {
        let (Some(old), Some(now)) = (album_row(expected, *id, 0)?, album_row(actual, *id, 0)?)
        else {
            continue;
        };
        if old == now {
            continue;
        }
        let before = typed::int(expected, &old, "revision")?;
        let after = typed::int(actual, &now, "revision")?;
        let mut desired = old.clone();
        desired[column] = Cell::Integer(after);
        ensure!(
            desired == now
                && typed::int(actual, &now, "deleted")? == 0
                && after > tombstone
                && numbers.insert(after),
            "album {id} revision change is unmodelled"
        );
        expected.rows.remove(&old);
        *expected.rows.entry(now).or_default() += 1;
        let stats = match (album_row(expected, *id, 1)?, album_row(actual, *id, 1)?) {
            (Some(row), None) => {
                let revision = typed::int(expected, &row, "revision")?;
                expected.rows.remove(&row);
                Some(revision)
            }
            _ => None,
        };
        albums.insert(*id, (before, after, stats));
    }
    // A later run's survivor revisions may have been renumbered again since
    // the session; the caller's between-runs rule decides them.
    if later {
        return Ok((Vec::new(), albums));
    }
    if expected.rows == actual.rows {
        contiguous(&numbers, tombstone)?;
        return Ok((Vec::new(), albums));
    }
    let old = live(expected)?;
    let new = live(actual)?;
    let mut expected_other: Rows = expected.rows.clone();
    let mut actual_other: Rows = actual.rows.clone();
    let mut changes = Vec::new();
    for (id, row) in &old {
        let now = &new[id];
        let before = typed::int(expected, row, "revision")?;
        let after = typed::int(actual, now, "revision")?;
        ensure!(
            after > before && numbers.insert(after),
            "revision refresh must advance every survivor uniquely"
        );
        let mut desired = row.clone();
        desired[column] = Cell::Integer(after);
        ensure!(
            desired == *now,
            "revision refresh changed a non-revision cell"
        );
        ensure!(
            expected_other.remove(row) == Some(1) && actual_other.remove(now) == Some(1),
            "revision refresh multiplicity differs"
        );
        changes.push(RevisionRefresh {
            item_pid: *id,
            before,
            after,
        });
    }
    ensure!(
        expected_other == actual_other,
        "revision refresh changed another revision row"
    );
    contiguous(&numbers, tombstone)?;
    for (id, row) in old {
        ensure!(
            expected.rows.remove(&row) == Some(1),
            "revision refresh row not unique"
        );
        *expected.rows.entry(new[&id].clone()).or_default() += 1;
    }
    Ok((changes, albums))
}

fn contiguous(numbers: &BTreeSet<i64>, tombstone: i64) -> Result<()> {
    for (offset, revision) in numbers.iter().enumerate() {
        ensure!(
            tombstone.checked_add(i64::try_from(offset)? + 1) == Some(*revision),
            "revision refresh is not contiguous above the tombstone"
        );
    }
    Ok(())
}

fn table<'a>(tables: &'a Tables, name: &str) -> Result<&'a Table> {
    tables
        .get(name)
        .with_context(|| format!("missing table {name}"))
}
fn item_ids(items: &Table) -> Result<BTreeSet<i64>> {
    let mut ids = BTreeSet::new();
    for (row, n) in &items.rows {
        ensure!(
            *n == 1 && ids.insert(typed::int(items, row, "item_pid")?),
            "duplicate item identity"
        );
    }
    Ok(ids)
}
fn target_revision(tables: &Tables, item: i64) -> Result<(&Row, i64)> {
    let t = table(tables, "entity_revision")?;
    let mut matches = t.rows.iter().filter(|(r, _)| {
        typed::int(t, r, "entity_pid").ok() == Some(item)
            && typed::int(t, r, "class").ok() == Some(0)
            && typed::int(t, r, "revision_type").ok() == Some(0)
    });
    let (row, n) = matches.next().context("target revision missing")?;
    ensure!(
        *n == 1 && matches.next().is_none(),
        "ambiguous target revision"
    );
    Ok((row, typed::int(t, row, "revision")?))
}
fn target_stats_revision(tables: &Tables, item: i64) -> Result<Option<(&Row, i64)>> {
    let t = table(tables, "entity_revision")?;
    let mut found = None;
    for (row, n) in &t.rows {
        if typed::int(t, row, "entity_pid")? != item || typed::int(t, row, "class")? != 0 {
            continue;
        }
        match typed::int(t, row, "revision_type")? {
            0 => {}
            1 => {
                ensure!(
                    *n == 1 && found.is_none() && typed::int(t, row, "deleted")? == 0,
                    "ambiguous or non-live target stats revision"
                );
                found = Some((row, typed::int(t, row, "revision")?));
            }
            other => bail!("unmodelled target revision type {other}"),
        }
    }
    Ok(found)
}
fn property_row<'a>(tables: &'a Tables, key: &str) -> Result<&'a Row> {
    typed::unique(
        table(tables, "_MLDatabaseProperties")?,
        "key",
        Cell::Text(key.into()),
    )
}
fn property<'a>(tables: &'a Tables, key: &str) -> Result<&'a str> {
    let t = table(tables, "_MLDatabaseProperties")?;
    t.at(property_row(tables, key)?, "value")?.text()
}
fn maintenance_date(value: &str) -> Result<f64> {
    ensure!(
        value.split('.').count() <= 2
            && value
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
        "maintenance date is not nonnegative decimal text"
    );
    let date: f64 = value.parse().context("invalid maintenance date")?;
    ensure!(date.is_finite(), "invalid maintenance date");
    Ok(date)
}
fn decimal(value: &str, signed: bool) -> Result<i64> {
    let n = value.parse::<i64>().context("invalid decimal property")?;
    ensure!(
        n.to_string() == value && (signed || n >= 0),
        "noncanonical decimal property"
    );
    Ok(n)
}
pub mod playlists;
