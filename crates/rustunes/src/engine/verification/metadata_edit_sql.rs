//! Expected complete SQL effect of one native metadata-only edit session
//! (`update_track` operations; docs/05-receiver-operations.md, section 2).
//!
//! The model starts from the before tables, applies each update in order and
//! requires every table to equal the after tables exactly. Only these effects
//! are modelled, all derived from the 41 observed receiver sessions:
//!
//! * Item fields land in `item`, `item_extra`, `item_state` (the receiver's
//!   triggers mirror title, location and explicit content from `item_extra`)
//!   and `item_search`; every order and section equals the `sort_map` row of
//!   the sent string, and strings new to the library add one `sort_map` row.
//! * A group (artist, album artist, album, composer, genre) is joined or
//!   created. The receiver matches artists, composers and genres by grouping
//!   key (derived from `sort_map.sort_key`), albums and album artists by their
//!   wire sync id. A join is accepted only into a row whose stored name
//!   already equals the sent text, because the receiver rewrites the name of
//!   the row it joins. New rows take the phone-allocated id and grouping key
//!   from the after state; every other cell must match the model.
//! * Representatives: a leaving representative hands over to the lowest
//!   remaining member (or zero), a joiner becomes the representative of an
//!   empty row, and a group created in this session ends with its lowest
//!   member.
//! * Revisions are allocated for the whole session: first every touched
//!   group once, by class (album artist, album, artist, composer, genre),
//!   then each update's item in operation order, where a later update of the
//!   same item overwrites the earlier number. Within one class the phone's
//!   order is taken from the after state (one session numbered a new album
//!   before an album an earlier update had revised). The block
//!   starts directly above the previous maximum revision. The anchor advances by one,
//!   last-synced and the hero revision equal the highest new revision and the
//!   generation id changes.
//! * Every nonzero recent play or skip counter may reset to zero, the
//!   maintenance date may advance, and an entry the phone added itself may
//!   have its state filled in as a synced track's (`fill_phone_entries` in
//!   the removal model, which saw it).
//! * A later run's capture (`verify_later`) may also hold what the phone did
//!   since the session: plays and skips, Playback History, its own
//!   renumbering of live items above the session's block and superseded
//!   statistics revisions (`Window::LaterRun`).
//! * An album the edit leaves alone that is still at keep_local_status 0 (an
//!   insert batch killed before its session ended leaves its new album so)
//!   may be evaluated: status 2, every other cell equal, its live revision
//!   moved above the session's own block and its stats revision row gone
//!   (seen after an insert batch killed mid-upload was compensated).
//!
//! Any other difference refuses; the caller keeps the full delta for review.
use crate::{
    engine::recovery::retained::playback::Window,
    engine::verification::sql_tables::reader::{Budget, Cell, Row, Table},
    engine::{
        planning::metadata_edit::{Class, Edit, Tables, conform, grouping_key},
        verification::{recent_counters, sql_values as typed},
    },
};
use anyhow::{Context, Result, bail, ensure};
use music_protocol::catalogue::{metadata_edit::decode, removal::fill_phone_entries};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const ANCHOR: &str = "MLSyncClientSyncAnchor";
const LAST_SYNCED: &str = "MLSyncClientLastSyncedRevision";
const GENERATION: &str = "MLSyncClientGenerationID";
const HERO: &str = "MLArtistHeroImageImportDatabaseRevision";
const MAINTENANCE: &str = "MLMaintenanceTaskOperationDate";
const ITEM_REVISION_CLASS: i64 = 0;
const SCHEMA: &str = "rustunes.metadata-edit-sql";

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RepresentativeMove {
    class: &'static str,
    group_pid: i64,
    before: i64,
    after: i64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GroupOutcome {
    class: &'static str,
    outcome: &'static str,
    group_pid: i64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct YearRecompute {
    album_pid: i64,
    before: i64,
    after: i64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NewRevision {
    entity: &'static str,
    entity_pid: i64,
    revision: i64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateReceipt {
    track: String,
    item_pid: i64,
    fields: Vec<String>,
    groups: Vec<GroupOutcome>,
    representatives: Vec<RepresentativeMove>,
    album_year: Option<YearRecompute>,
    revisions: Vec<NewRevision>,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MetadataEditReceipt {
    schema: &'static str,
    base_anchor: u32,
    revision: u32,
    base_revision: i64,
    highest_revision: i64,
    updates: Vec<UpdateReceipt>,
    sort_map_additions: Vec<String>,
    property_changes: BTreeMap<&'static str, (String, String)>,
    recent_counter_resets: Vec<recent_counters::Reset>,
    keep_local_evaluations: Vec<KeepLocalEvaluation>,
    /// Entries the phone added itself whose state the session filled in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    filled_phone_entries: Vec<i64>,
    /// What a later run's capture showed of the phone's use since the
    /// session (`verify_later`).
    #[serde(skip_serializing_if = "Option::is_none")]
    between_runs_playback: Option<serde_json::Value>,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct KeepLocalEvaluation {
    album_pid: i64,
    revision: (i64, i64),
    removed_stats_revision: Option<i64>,
}
impl MetadataEditReceipt {
    pub(crate) fn updated(&self) -> usize {
        self.updates.len()
    }
    #[cfg(test)]
    pub(crate) fn highest_revision(&self) -> i64 {
        self.highest_revision
    }
}

fn table<'a>(tables: &'a Tables, name: &str) -> Result<&'a Table> {
    tables
        .get(name)
        .with_context(|| format!("missing table {name}"))
}
fn property_row<'a>(tables: &'a Tables, key: &str) -> Result<&'a Row> {
    typed::unique(
        table(tables, "_MLDatabaseProperties")?,
        "key",
        Cell::Text(key.into()),
    )
}
fn property<'a>(tables: &'a Tables, key: &str) -> Result<&'a str> {
    table(tables, "_MLDatabaseProperties")?
        .at(property_row(tables, key)?, "value")?
        .text()
}
fn decimal(value: &str, signed: bool) -> Result<i64> {
    let n = value.parse::<i64>().context("invalid decimal property")?;
    ensure!(
        n.to_string() == value && (signed || n >= 0),
        "noncanonical decimal property"
    );
    Ok(n)
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
fn int(v: i64) -> Cell {
    Cell::Integer(v)
}
fn text(v: &str) -> Cell {
    Cell::Text(v.to_owned())
}

/// One working row: loaded from the expected tables, edited, then stored back.
struct Rec {
    name: &'static str,
    columns: Vec<String>,
    old: Row,
    row: Row,
}
impl Rec {
    fn load(tables: &Tables, name: &'static str, key: &str, id: i64) -> Result<Self> {
        let t = table(tables, name)?;
        let old = t.one(key, id)?.clone();
        ensure!(t.rows.get(&old) == Some(&1), "duplicate {name} row");
        Ok(Self {
            name,
            columns: t.columns.clone(),
            row: old.clone(),
            old,
        })
    }
    fn index(&self, column: &str) -> Result<usize> {
        self.columns
            .iter()
            .position(|c| c == column)
            .with_context(|| format!("missing column {}.{column}", self.name))
    }
    fn set(&mut self, column: &str, value: Cell) -> Result<()> {
        let i = self.index(column)?;
        self.row[i] = value;
        Ok(())
    }
    fn get(&self, column: &str) -> Result<&Cell> {
        Ok(&self.row[self.index(column)?])
    }
    fn int(&self, column: &str) -> Result<i64> {
        self.get(column)?.integer()
    }
    fn store(self, tables: &mut Tables) -> Result<()> {
        typed::replace(tables, self.name, &self.old, self.row)
    }
}

struct Resolved {
    pid: i64,
    created: bool,
}
/// What a new group row must contain besides the after state's own id.
struct NewGroup<'a> {
    name: &'a str,
    sort: Option<&'a str>,
    representative: i64,
    key: Vec<u8>,
    sync: i64,
    parent: i64,
    year: i64,
    order: (i64, i64),
}

fn rank(class: Class, genre_first: bool) -> u8 {
    match class {
        Class::AlbumArtist => 0,
        Class::Album => 1,
        Class::ItemArtist => 2,
        Class::Composer => {
            if genre_first {
                4
            } else {
                3
            }
        }
        Class::Genre => {
            if genre_first {
                3
            } else {
                4
            }
        }
    }
}

struct Model<'a> {
    expected: Tables,
    after: &'a Tables,
    counter: i64,
    created: BTreeSet<(Class, i64)>,
    revised: BTreeSet<(i64, i64)>,
    receipts: Vec<UpdateReceipt>,
    /// Groups needing a revision, with the update that touched them first.
    touched: Vec<(Class, i64, usize)>,
}

impl Model<'_> {
    fn t(&self, name: &str) -> Result<&Table> {
        table(&self.expected, name)
    }
    fn load(&self, name: &'static str, key: &str, id: i64) -> Result<Rec> {
        Rec::load(&self.expected, name, key, id)
    }
    fn group(&self, class: Class, pid: i64) -> Result<Rec> {
        self.load(class.table(), class.key(), pid)
            .with_context(|| format!("{} group {pid} is missing", class.label()))
    }
    fn order(&self, name: &str) -> Result<(i64, i64)> {
        let t = self.t("sort_map")?;
        let row = typed::unique(t, "name", text(name))
            .with_context(|| format!("sort_map has no row for {name:?}"))?;
        Ok((
            typed::int(t, row, "name_order")?,
            typed::int(t, row, "name_section")?,
        ))
    }
    fn key_of(&self, name: &str) -> Result<Vec<u8>> {
        let t = self.t("sort_map")?;
        let row = typed::unique(t, "name", text(name))
            .with_context(|| format!("sort_map has no row for {name:?}"))?;
        let Cell::Blob(sort_key) = t.at(row, "sort_key")? else {
            bail!("sort_map key for {name:?} is not a blob");
        };
        grouping_key(sort_key)
            .map(<[u8]>::to_vec)
            .with_context(|| format!("sort_map key for {name:?} has no grouping key"))
    }
    /// Items currently in a group, optionally without one of them.
    fn members(&self, class: Class, group: i64, exclude: Option<i64>) -> Result<BTreeSet<i64>> {
        let items = self.t("item")?;
        let column = items.column(class.key())?;
        let pid = items.column("item_pid")?;
        let mut found = BTreeSet::new();
        for row in items.rows.keys() {
            if row[column] == Cell::Integer(group) {
                let id = row[pid].integer()?;
                if Some(id) != exclude {
                    found.insert(id);
                }
            }
        }
        Ok(found)
    }

    /// Next revision for an entity; a later touch overwrites the row.
    fn allocate(&mut self, class: i64, entity: i64, new_entity: bool) -> Result<i64> {
        let first = self.revised.insert((class, entity));
        let fresh = new_entity && first;
        self.counter = self
            .counter
            .checked_add(1)
            .context("revision counter exhausted")?;
        let revision = self.counter;
        let t = self.t("entity_revision")?;
        let (pid, kind, type_, rev) = (
            t.column("entity_pid")?,
            t.column("class")?,
            t.column("revision_type")?,
            t.column("revision")?,
        );
        let existing: Vec<Row> = t
            .rows
            .keys()
            .filter(|r| r[pid] == int(entity) && r[kind] == int(class) && r[type_] == int(0))
            .cloned()
            .collect();
        match existing.as_slice() {
            [] => {
                ensure!(
                    fresh,
                    "entity {entity} (class {class}) has no revision row to replace"
                );
                let mut row = vec![int(0); t.columns.len()];
                row[rev] = int(revision);
                row[pid] = int(entity);
                row[kind] = int(class);
                row[type_] = int(0);
                row[t.column("deleted")?] = int(0);
                ensure!(
                    t.columns.len() == 5,
                    "entity_revision has unmodelled columns"
                );
                let t = self
                    .expected
                    .get_mut("entity_revision")
                    .context("revisions")?;
                ensure!(t.rows.insert(row, 1).is_none(), "revision row exists");
            }
            [old] => {
                ensure!(
                    !fresh && t.rows.get(old) == Some(&1) && t.at(old, "deleted")? == &int(0),
                    "entity {entity} revision row is not a single live row"
                );
                let mut row = old.clone();
                row[rev] = int(revision);
                typed::replace(&mut self.expected, "entity_revision", old, row)?;
            }
            _ => bail!("entity {entity} has several revision rows"),
        }
        Ok(revision)
    }

    /// Representative bookkeeping when an item moves between group rows.
    fn relocate(
        &mut self,
        class: Class,
        item: i64,
        old: i64,
        new: i64,
        created: bool,
        moves: &mut Vec<RepresentativeMove>,
    ) -> Result<()> {
        if old != 0 {
            let mut group = self.group(class, old)?;
            let was = group.int("representative_item_pid")?;
            if was == item {
                let remaining = self.members(class, old, Some(item))?;
                let now = remaining.first().copied().unwrap_or(0);
                group.set("representative_item_pid", int(now))?;
                group.store(&mut self.expected)?;
                moves.push(RepresentativeMove {
                    class: class.table(),
                    group_pid: old,
                    before: was,
                    after: now,
                });
            }
        }
        if new != 0 && !created {
            let mut group = self.group(class, new)?;
            if self.members(class, new, Some(item))?.is_empty() {
                let was = group.int("representative_item_pid")?;
                if was != item {
                    group.set("representative_item_pid", int(item))?;
                    group.store(&mut self.expected)?;
                    moves.push(RepresentativeMove {
                        class: class.table(),
                        group_pid: new,
                        before: was,
                        after: item,
                    });
                }
            }
        }
        Ok(())
    }

    /// The value of every cell of a newly created group row. Columns outside
    /// this list make the verifier refuse (an unknown schema is not modelled).
    fn new_cell(&self, class: Class, column: &str, id: &Cell, g: &NewGroup<'_>) -> Result<Cell> {
        if column == class.key() {
            return Ok(id.clone());
        }
        if column == class.name() {
            return Ok(text(g.name));
        }
        if Some(column) == class.sort() {
            return Ok(text(g.sort.context("group has no sort name")?));
        }
        let zero = int(0);
        Ok(match (class, column) {
            (_, "grouping_key") => Cell::Blob(g.key.clone()),
            (_, "representative_item_pid") => int(g.representative),
            (_, "sync_id") => int(g.sync),
            (_, "app_data") => Cell::Null,
            (
                _,
                "cloud_status"
                | "keep_local"
                | "keep_local_status"
                | "keep_local_status_reason"
                | "keep_local_constraints",
            ) => zero,
            (
                Class::ItemArtist | Class::AlbumArtist | Class::Album,
                "store_id" | "classical_experience_available",
            ) => zero,
            (Class::ItemArtist, "series_name") => text(""),
            (Class::ItemArtist, "sort_series_name") => Cell::Null,
            (Class::AlbumArtist, "cloud_universal_library_id") => text(""),
            (Class::AlbumArtist | Class::Album, "liked_state" | "liked_state_changed_date") => zero,
            (Class::AlbumArtist, "sort_order" | "name_order") => int(g.order.0),
            (Class::AlbumArtist, "sort_order_section") => int(g.order.1),
            (Class::Album, "album_artist_pid") => int(g.parent),
            (Class::Album, "album_year") => int(g.year),
            (Class::Album, "album_type") => int(1),
            (
                Class::Album,
                "all_compilations"
                | "user_rating"
                | "season_number"
                | "contains_classical_work"
                | "date_played_local"
                | "user_rating_is_derived"
                | "date_released"
                | "is_prerelease"
                | "release_event_aux_info"
                | "content_timestamp"
                | "expected_release_date"
                | "is_followed",
            ) => zero,
            (
                Class::Album,
                "feed_url" | "cloud_library_id" | "editorial_notes" | "cloud_channel_name",
            ) => text(""),
            _ => bail!(
                "new {} row has the unmodelled column {column}",
                class.table()
            ),
        })
    }
}
