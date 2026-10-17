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

    /// Find the phone-created row in the after state, require it to equal the
    /// model, and add it to the expected tables.
    fn create(&mut self, class: Class, g: &NewGroup<'_>) -> Result<i64> {
        let after = table(self.after, class.table())?;
        let existing = self.t(class.table())?;
        let key_column = existing.column(class.key())?;
        let known: BTreeSet<i64> = existing
            .rows
            .keys()
            .map(|r| r[key_column].integer())
            .collect::<Result<_>>()?;
        ensure!(
            existing.columns == after.columns,
            "{} columns differ",
            class.table()
        );
        let name_column = after.column(class.name())?;
        let sync_column = after.column("sync_id")?;
        let key_cell = after.column("grouping_key")?;
        let mut candidates = Vec::new();
        for (row, n) in &after.rows {
            let id = row[key_column].integer()?;
            if known.contains(&id) {
                continue;
            }
            ensure!(*n == 1, "duplicate new {} row", class.table());
            let by_sync = matches!(class, Class::Album | Class::AlbumArtist);
            if row[name_column] == text(g.name)
                && (!by_sync || row[sync_column] == int(g.sync))
                && row[key_cell] == Cell::Blob(g.key.clone())
            {
                candidates.push(row);
            }
        }
        ensure!(
            candidates.len() == 1,
            "expected exactly one new {} row for {:?}, found {}",
            class.table(),
            g.name,
            candidates.len()
        );
        let observed = candidates[0].clone();
        let id = observed[key_column].clone();
        ensure!(
            matches!(id, Cell::Integer(n) if n != 0),
            "new {} id is not a nonzero integer",
            class.table()
        );
        let mut built = Vec::with_capacity(after.columns.len());
        for column in &after.columns {
            built.push(self.new_cell(class, column, &id, g)?);
        }
        for (i, column) in after.columns.iter().enumerate() {
            // The final representative of a row several updates join is the
            // lowest member; the whole-table comparison checks it at the end.
            if column == "representative_item_pid" {
                continue;
            }
            ensure!(
                built[i] == observed[i],
                "new {} row {:?} differs from the model in {column}: expected {:?}, found {:?}",
                class.table(),
                g.name,
                built[i],
                observed[i]
            );
        }
        let pid = id.integer()?;
        let t = self
            .expected
            .get_mut(class.table())
            .context("group table")?;
        ensure!(t.rows.insert(built, 1).is_none(), "new group row exists");
        self.created.insert((class, pid));
        Ok(pid)
    }

    /// Artist, composer and genre rows are matched by grouping key.
    fn resolve_by_key(
        &mut self,
        class: Class,
        name: &str,
        sort: Option<&str>,
        item: i64,
        year: i64,
    ) -> Result<Resolved> {
        let key = self.key_of(name)?;
        let t = self.t(class.table())?;
        let key_column = t.column("grouping_key")?;
        let hits: Vec<Row> = t
            .rows
            .keys()
            .filter(|r| r[key_column] == Cell::Blob(key.clone()))
            .cloned()
            .collect();
        match hits.as_slice() {
            [] => {
                let pid = self.create(
                    class,
                    &NewGroup {
                        name,
                        sort,
                        representative: item,
                        key,
                        sync: 0,
                        parent: 0,
                        year,
                        order: (0, 0),
                    },
                )?;
                Ok(Resolved { pid, created: true })
            }
            [row] => {
                self.joinable(class, t, row, name, sort)?;
                Ok(Resolved {
                    pid: typed::int(t, row, class.key())?,
                    created: false,
                })
            }
            _ => bail!("{} {name:?} matches several groups by key", class.label()),
        }
    }

    /// A join must not rewrite the joined row's name or sort name.
    fn joinable(
        &self,
        class: Class,
        t: &Table,
        row: &Row,
        name: &str,
        sort: Option<&str>,
    ) -> Result<()> {
        let stored = t.at(row, class.name())?.text()?;
        ensure!(
            stored == name,
            "{} {name:?} would join the group named {stored:?} and rewrite its name",
            class.label()
        );
        if let (Some(column), Some(sort)) = (class.sort(), sort) {
            let stored_sort = match t.at(row, column)? {
                Cell::Text(s) if !s.is_empty() => s.as_str(),
                Cell::Text(_) | Cell::Null => stored,
                _ => bail!("{column} is not text"),
            };
            ensure!(
                stored_sort == sort,
                "{} {name:?} has sort name {stored_sort:?}, not {sort:?}",
                class.label()
            );
        }
        Ok(())
    }

    /// Albums and album artists are matched by the wire sync id.
    fn resolve_by_sync(
        &mut self,
        class: Class,
        sync: i64,
        name: &str,
        sort: &str,
        item: i64,
        parent: i64,
        year: i64,
    ) -> Result<Resolved> {
        let t = self.t(class.table())?;
        let sync_column = t.column("sync_id")?;
        let hits: Vec<Row> = t
            .rows
            .keys()
            .filter(|r| r[sync_column] == int(sync))
            .cloned()
            .collect();
        match hits.as_slice() {
            [] => {
                let key = self.key_of(name)?;
                let order = self.order(sort)?;
                let pid = self.create(
                    class,
                    &NewGroup {
                        name,
                        sort: Some(sort),
                        representative: item,
                        key,
                        sync,
                        parent,
                        year,
                        order,
                    },
                )?;
                Ok(Resolved { pid, created: true })
            }
            [row] => {
                self.joinable(class, t, row, name, Some(sort))?;
                if class == Class::Album {
                    ensure!(
                        typed::int(t, row, "album_artist_pid")? == parent,
                        "album {name:?} belongs to another album artist"
                    );
                }
                Ok(Resolved {
                    pid: typed::int(t, row, class.key())?,
                    created: false,
                })
            }
            _ => bail!("{} sync id {sync} matches several rows", class.label()),
        }
    }

    fn apply(&mut self, e: &Edit) -> Result<()> {
        let pid = e.item_pid;
        let mut item = self.load("item", "item_pid", pid)?;
        let mut extra = self.load("item_extra", "item_pid", pid)?;
        let mut search = self.load("item_search", "item_pid", pid)?;
        let mut groups = Vec::new();
        let mut moves = Vec::new();
        let mut touched: Vec<(Class, i64)> = Vec::new();

        if let Some((title, sort)) = &e.title {
            extra.set("title", text(title))?;
            extra.set("sort_title", text(sort))?;
            let (order, section) = self.order(sort)?;
            item.set("title_order", int(order))?;
            item.set("title_order_section", int(section))?;
            search.set("search_title", int(self.order(title)?.0))?;
        }
        if let Some(n) = e.track_number {
            item.set("track_number", int(n.into()))?;
            item.set("episode_sort_id", int(n.into()))?;
        }
        if let Some(n) = e.disc_number {
            item.set("disc_number", int(n.into()))?;
        }
        if let Some(n) = e.track_count {
            extra.set("track_count", int(n.into()))?;
        }
        if let Some(n) = e.disc_count {
            extra.set("disc_count", int(n.into()))?;
        }
        if let Some(n) = e.year {
            extra.set("year", int(n.into()))?;
        }
        if let Some(flag) = e.is_compilation {
            item.set("is_compilation", int(flag.into()))?;
        }
        if let Some(n) = e.content_rating {
            extra.set("content_rating", int(n.into()))?;
        }
        if let Some(copyright) = &e.copyright {
            extra.set("copyright", text(copyright))?;
        }
        if let Some(comment) = &e.comment {
            extra.set("comment", text(comment))?;
        }
        let year = extra.int("year")?;

        // Album artist (full set), album, artist, composer, genre, in the
        // order the receiver allocates their revisions.
        let mut aa_pid = item.int("album_artist_pid")?;
        let mut album_created = false;
        if let Some(full) = &e.album_artist {
            let found = self.resolve_by_sync(
                Class::AlbumArtist,
                full.pid as i64,
                &full.text,
                &full.sort,
                pid,
                0,
                year,
            )?;
            let old = item.int("album_artist_pid")?;
            if found.created {
                touched.push((Class::AlbumArtist, found.pid));
            }
            self.moved(
                Class::AlbumArtist,
                pid,
                old,
                &found,
                &mut groups,
                &mut moves,
            )?;
            item.set("album_artist_pid", int(found.pid))?;
            let (order, section) = self.order(&full.sort)?;
            item.set("album_artist_order", int(order))?;
            item.set("album_artist_order_section", int(section))?;
            search.set("search_album_artist", int(self.order(&full.text)?.0))?;
            aa_pid = found.pid;
        }
        if let Some(album) = &e.album {
            let found = self.resolve_by_sync(
                Class::Album,
                album.pid as i64,
                &album.text,
                &album.sort,
                pid,
                aa_pid,
                year,
            )?;
            let old = item.int("album_pid")?;
            if found.created {
                touched.push((Class::Album, found.pid));
                album_created = true;
            }
            self.moved(Class::Album, pid, old, &found, &mut groups, &mut moves)?;
            item.set("album_pid", int(found.pid))?;
            let (order, section) = self.order(&album.sort)?;
            item.set("album_order", int(order))?;
            item.set("album_order_section", int(section))?;
            search.set("search_album", int(self.order(&album.text)?.0))?;
        }
        if let Some((artist, sort)) = &e.artist {
            let found = self.resolve_by_key(Class::ItemArtist, artist, Some(sort), pid, year)?;
            let old = item.int("item_artist_pid")?;
            if found.created {
                touched.push((Class::ItemArtist, found.pid));
            }
            self.moved(Class::ItemArtist, pid, old, &found, &mut groups, &mut moves)?;
            item.set("item_artist_pid", int(found.pid))?;
            let (order, section) = self.order(sort)?;
            item.set("item_artist_order", int(order))?;
            item.set("item_artist_order_section", int(section))?;
            let shown = self.order(artist)?.0;
            search.set("search_artist", int(shown))?;
            if e.album_artist.is_none() {
                // Observed in every artist-only session: the receiver also
                // moves the album-artist search order to the new artist.
                search.set("search_album_artist", int(shown))?;
            }
        }
        if let Some((composer, sort)) = &e.composer {
            let found = self.resolve_by_key(Class::Composer, composer, Some(sort), pid, year)?;
            let old = item.int("composer_pid")?;
            if found.created {
                touched.push((Class::Composer, found.pid));
            }
            self.moved(Class::Composer, pid, old, &found, &mut groups, &mut moves)?;
            item.set("composer_pid", int(found.pid))?;
            let (order, section) = self.order(sort)?;
            item.set("composer_order", int(order))?;
            item.set("composer_order_section", int(section))?;
            search.set("search_composer", int(self.order(composer)?.0))?;
        }
        if let Some(genre) = &e.genre {
            let found = self.resolve_by_key(Class::Genre, genre, None, pid, year)?;
            let old = item.int("genre_id")?;
            if found.created {
                touched.push((Class::Genre, found.pid));
            }
            self.moved(Class::Genre, pid, old, &found, &mut groups, &mut moves)?;
            item.set("genre_id", int(found.pid))?;
            let (order, section) = self.order(genre)?;
            item.set("genre_order", int(order))?;
            item.set("genre_order_section", int(section))?;
        }

        // A year that differs from the album's gives the album a new revision
        // and recomputes its year as the members' maximum (seen in several
        // sessions, and a phone edit 2022 -> 2021); a year equal to the album's leaves the album
        // alone (2021 -> 2022 into a 2022 album). An album created by this
        // update already carries both.
        let year_album = match e.year {
            Some(year) if !album_created => {
                let album = item.int("album_pid")?;
                (album != 0
                    && self.group(Class::Album, album)?.int("album_year")? != i64::from(year))
                .then_some(album)
            }
            _ => None,
        };

        let extra_changed = extra.row != extra.old;
        let content_rating = extra.int("content_rating")?;
        let title_cell = extra.get("title")?.clone();
        let location_cell = extra.get("location")?.clone();
        item.store(&mut self.expected)?;
        extra.store(&mut self.expected)?;
        search.store(&mut self.expected)?;
        if extra_changed {
            // The receiver's item_extra triggers mirror these three cells.
            let mut state = self.load("item_state", "item_pid", pid)?;
            let nonnull = |c: Cell| if c == Cell::Null { text("") } else { c };
            state.set("title", nonnull(title_cell))?;
            state.set("location", nonnull(location_cell))?;
            state.set("has_explicit_content", int((content_rating == 1).into()))?;
            state.store(&mut self.expected)?;
        }
        let mut album_year = None;
        if let Some(album) = year_album {
            let items = self.members(Class::Album, album, None)?;
            let extras = self.t("item_extra")?;
            let mut newest = 0;
            for member in &items {
                let row = extras.one("item_pid", *member)?;
                newest = newest.max(typed::int(extras, row, "year")?);
            }
            let mut row = self.group(Class::Album, album)?;
            let before = row.int("album_year")?;
            row.set("album_year", int(newest))?;
            row.store(&mut self.expected)?;
            touched.push((Class::Album, album));
            album_year = Some(YearRecompute {
                album_pid: album,
                before,
                after: newest,
            });
        }

        // Revisions are allocated once every update is applied.
        let update = self.receipts.len();
        for (class, group) in touched {
            if !self.touched.iter().any(|t| (t.0, t.1) == (class, group)) {
                self.touched.push((class, group, update));
            }
        }
        self.receipts.push(UpdateReceipt {
            track: format!("{:#018x}", e.sync_id),
            item_pid: pid,
            fields: e.keys.clone(),
            groups,
            representatives: moves,
            album_year,
            revisions: Vec::new(),
        });
        Ok(())
    }

    fn moved(
        &mut self,
        class: Class,
        item: i64,
        old: i64,
        found: &Resolved,
        groups: &mut Vec<GroupOutcome>,
        moves: &mut Vec<RepresentativeMove>,
    ) -> Result<()> {
        groups.push(GroupOutcome {
            class: class.table(),
            outcome: if found.created {
                "created"
            } else if found.pid == old {
                "unchanged"
            } else {
                "joined"
            },
            group_pid: found.pid,
        });
        if found.pid != old {
            self.relocate(class, item, old, found.pid, found.created, moves)?;
        }
        Ok(())
    }
}
