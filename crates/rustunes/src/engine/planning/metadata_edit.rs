//! Offline planner and conformance gate for native metadata-only edits.
//!
//! Evidence and recipe: docs/05-receiver-operations.md, section 2. The receiver
//! applies item fields by themselves but regroups artist, album artist, album,
//! composer and genre by its own rules, and several requests that look
//! harmless damage shared groups (an artist edit of a non-representative track
//! renames the shared group in place; an album name without `album_pid` leaves
//! the catalogue inconsistent). This module therefore turns explicit requests
//! into only the operations the recipe allows, and `check_conformant` accepts
//! exactly that set so the SQL verifier can refuse anything else before it
//! models the effect. Nothing here contacts a device or authorizes execution.
use crate::{
    engine::verification::sql_tables::reader::Table, engine::verification::sql_values as typed,
};
use anyhow::{Context, Result, bail, ensure};
use music_protocol::catalogue::metadata_edit::{TrackUpdate, validate};
use plist::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) type Tables = BTreeMap<String, Table>;

/// Explicit user intent for one track. `None` preserves; sort values default
/// to the new name when the name changes and the sort is omitted. An empty
/// copyright or comment and a 0 year, track number, track count or disc count
/// clear the field. Other empty strings are refused: the receiver keeps a
/// genre or composer sent as `""`.
#[derive(Clone, Debug, Default)]
pub(crate) struct EditRequest {
    /// `item_store.sync_id` of the track.
    pub track: u64,
    pub title: Option<String>,
    pub sort_title: Option<String>,
    pub artist: Option<String>,
    pub sort_artist: Option<String>,
    pub album: Option<String>,
    pub sort_album: Option<String>,
    pub album_artist: Option<String>,
    pub sort_album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub sort_composer: Option<String>,
    pub year: Option<u16>,
    pub track_number: Option<u16>,
    pub track_count: Option<u16>,
    pub disc_number: Option<u16>,
    pub disc_count: Option<u16>,
    pub is_compilation: Option<bool>,
    pub content_rating: Option<u8>,
    pub copyright: Option<String>,
    /// Line breaks and tabs are kept.
    pub comment: Option<String>,
}

/// The five grouping tables whose rows hold a stored representative item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Class {
    ItemArtist,
    AlbumArtist,
    Album,
    Composer,
    Genre,
}
impl Class {
    pub(crate) const ALL: [Class; 5] = [
        Class::ItemArtist,
        Class::AlbumArtist,
        Class::Album,
        Class::Composer,
        Class::Genre,
    ];
    pub(crate) fn table(self) -> &'static str {
        match self {
            Class::ItemArtist => "item_artist",
            Class::AlbumArtist => "album_artist",
            Class::Album => "album",
            Class::Composer => "composer",
            Class::Genre => "genre",
        }
    }
    /// Primary key of the group table; the `item` table uses the same name.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Class::ItemArtist => "item_artist_pid",
            Class::AlbumArtist => "album_artist_pid",
            Class::Album => "album_pid",
            Class::Composer => "composer_pid",
            Class::Genre => "genre_id",
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Class::ItemArtist => "item_artist",
            Class::AlbumArtist => "album_artist",
            Class::Album => "album",
            Class::Composer => "composer",
            Class::Genre => "genre",
        }
    }
    pub(crate) fn sort(self) -> Option<&'static str> {
        match self {
            Class::ItemArtist => Some("sort_item_artist"),
            Class::AlbumArtist => Some("sort_album_artist"),
            Class::Album => Some("sort_album"),
            Class::Composer => Some("sort_composer"),
            Class::Genre => None,
        }
    }
    /// `entity_revision.class` of the group's own revision row.
    pub(crate) fn revision_class(self) -> i64 {
        match self {
            Class::ItemArtist => 2,
            Class::Genre => 3,
            Class::Album => 4,
            Class::Composer => 5,
            Class::AlbumArtist => 7,
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Class::ItemArtist => "artist",
            Class::AlbumArtist => "album artist",
            Class::Album => "album",
            Class::Composer => "composer",
            Class::Genre => "genre",
        }
    }
}

/// The grouping key the receiver derives from a name: the bytes of its
/// `sort_map.sort_key` after the leading section byte and before the first
/// 0x01 separator. Every consistent group row in the evidence satisfies
/// `grouping_key == grouping_key(sort_map[name].sort_key)`.
pub(crate) fn grouping_key(sort_key: &[u8]) -> Option<&[u8]> {
    let body = sort_key.get(1..)?;
    let end = body.iter().position(|b| *b == 1).unwrap_or(body.len());
    (end > 0).then(|| &body[..end])
}

/// Whether the receiver may file two names under one grouping key, for a name
/// `sort_map` does not know yet; it then merges them and rewrites the display
/// name. The key ignores case, but the phone's keys set spaces and
/// punctuation apart (`Headliner/ Guest` from `Headliner, Guest`, `Northside`
/// from `NORTH SIDE`), so two printable ASCII names merge only when they differ
/// in case or in runs of spaces. Beyond printable ASCII the collation is
/// unknown: any two names with the same letters and digits may merge.
pub(crate) fn may_merge(a: &str, b: &str) -> bool {
    let printable = |text: &str| text.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    if printable(a) && printable(b) {
        let words = |text: &str| {
            text.split_ascii_whitespace()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        };
        return words(a) == words(b);
    }
    let fold = |text: &str| {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    fold(a) == fold(b)
}

#[derive(Clone, Debug)]
pub(crate) struct ItemRec {
    pub pid: i64,
    pub item_artist: i64,
    pub album_artist: i64,
    pub album: i64,
    pub composer: i64,
    pub genre: i64,
    pub compilation: bool,
    pub track_number: i64,
    pub disc_number: i64,
}
#[derive(Clone, Debug)]
pub(crate) struct ExtraRec {
    pub title: String,
    pub sort_title: String,
    pub track_count: i64,
    pub disc_count: i64,
    pub year: i64,
    pub content_rating: i64,
    pub copyright: String,
    /// NULL reads as empty: a cleared comment is stored as `''`.
    pub comment: String,
}
impl ExtraRec {
    fn sort_effective(&self) -> &str {
        if self.sort_title.is_empty() {
            &self.title
        } else {
            &self.sort_title
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct GroupRec {
    pub name: String,
    pub sort: Option<String>,
    pub rep: i64,
    pub key: Option<Vec<u8>>,
    pub sync: i64,
    /// `album.album_artist_pid`; zero for the other classes.
    pub parent: i64,
    /// `album.all_compilations`; zero for the other classes.
    pub all_compilations: i64,
}
impl GroupRec {
    fn sort_effective(&self) -> &str {
        match &self.sort {
            Some(sort) if !sort.is_empty() => sort,
            _ => &self.name,
        }
    }
}

/// Indexed facts of a before state. Reading refuses duplicate identities.
pub(crate) struct Catalogue {
    pub items: BTreeMap<i64, ItemRec>,
    pub extras: BTreeMap<i64, ExtraRec>,
    by_sync: BTreeMap<i64, Vec<i64>>,
    groups: BTreeMap<Class, BTreeMap<i64, GroupRec>>,
    sort_keys: BTreeMap<String, Vec<u8>>,
    /// Every nonzero `album`, `album_artist` and `item_store` sync id.
    pub used: BTreeSet<i64>,
}

fn table<'a>(tables: &'a Tables, name: &str) -> Result<&'a Table> {
    tables
        .get(name)
        .with_context(|| format!("missing table {name}"))
}
fn text_cell(
    t: &Table,
    row: &crate::engine::verification::sql_tables::reader::Row,
    column: &str,
) -> Result<String> {
    use crate::engine::verification::sql_tables::reader::Cell;
    match t.at(row, column)? {
        Cell::Text(value) => Ok(value.clone()),
        Cell::Null => Ok(String::new()),
        _ => bail!("{column} is not text"),
    }
}
fn opt_text_cell(
    t: &Table,
    row: &crate::engine::verification::sql_tables::reader::Row,
    column: &str,
) -> Result<Option<String>> {
    use crate::engine::verification::sql_tables::reader::Cell;
    match t.at(row, column)? {
        Cell::Text(value) => Ok(Some(value.clone())),
        Cell::Null => Ok(None),
        _ => bail!("{column} is not text"),
    }
}
fn key_cell(
    t: &Table,
    row: &crate::engine::verification::sql_tables::reader::Row,
    column: &str,
) -> Result<Option<Vec<u8>>> {
    use crate::engine::verification::sql_tables::reader::Cell;
    match t.at(row, column)? {
        Cell::Blob(value) => Ok(Some(value.clone())),
        Cell::Null => Ok(None),
        _ => bail!("{column} is not a blob"),
    }
}

impl Catalogue {
    pub(crate) fn read(tables: &Tables) -> Result<Self> {
        let mut used = BTreeSet::new();
        let items_table = table(tables, "item")?;
        let extra_table = table(tables, "item_extra")?;
        let store_table = table(tables, "item_store")?;
        let mut items = BTreeMap::new();
        for (row, n) in &items_table.rows {
            ensure!(*n == 1, "duplicate item row");
            let pid = typed::int(items_table, row, "item_pid")?;
            let rec = ItemRec {
                pid,
                item_artist: typed::int(items_table, row, "item_artist_pid")?,
                album_artist: typed::int(items_table, row, "album_artist_pid")?,
                album: typed::int(items_table, row, "album_pid")?,
                composer: typed::int(items_table, row, "composer_pid")?,
                genre: typed::int(items_table, row, "genre_id")?,
                compilation: typed::int(items_table, row, "is_compilation")? != 0,
                track_number: typed::int(items_table, row, "track_number")?,
                disc_number: typed::int(items_table, row, "disc_number")?,
            };
            ensure!(items.insert(pid, rec).is_none(), "duplicate item identity");
        }
        let mut extras = BTreeMap::new();
        for (row, n) in &extra_table.rows {
            ensure!(*n == 1, "duplicate item_extra row");
            let pid = typed::int(extra_table, row, "item_pid")?;
            let rec = ExtraRec {
                title: text_cell(extra_table, row, "title")?,
                sort_title: text_cell(extra_table, row, "sort_title")?,
                track_count: typed::int(extra_table, row, "track_count")?,
                disc_count: typed::int(extra_table, row, "disc_count")?,
                year: typed::int(extra_table, row, "year")?,
                content_rating: typed::int(extra_table, row, "content_rating")?,
                copyright: text_cell(extra_table, row, "copyright")?,
                comment: text_cell(extra_table, row, "comment")?,
            };
            ensure!(
                extras.insert(pid, rec).is_none(),
                "duplicate item_extra identity"
            );
        }
        let mut by_sync: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        let mut stored = BTreeSet::new();
        for (row, n) in &store_table.rows {
            ensure!(*n == 1, "duplicate item_store row");
            let pid = typed::int(store_table, row, "item_pid")?;
            ensure!(stored.insert(pid), "duplicate item_store identity");
            let sync = typed::int(store_table, row, "sync_id")?;
            if sync != 0 {
                by_sync.entry(sync).or_default().push(pid);
                used.insert(sync);
            }
        }
        let mut groups = BTreeMap::new();
        for class in Class::ALL {
            let t = table(tables, class.table())?;
            let mut found = BTreeMap::new();
            for (row, n) in &t.rows {
                ensure!(*n == 1, "duplicate {} row", class.table());
                let pid = typed::int(t, row, class.key())?;
                let rec = GroupRec {
                    name: text_cell(t, row, class.name())?,
                    sort: match class.sort() {
                        Some(column) => opt_text_cell(t, row, column)?,
                        None => None,
                    },
                    rep: typed::int(t, row, "representative_item_pid")?,
                    key: key_cell(t, row, "grouping_key")?,
                    sync: typed::int(t, row, "sync_id")?,
                    parent: if class == Class::Album {
                        typed::int(t, row, "album_artist_pid")?
                    } else {
                        0
                    },
                    all_compilations: if class == Class::Album {
                        typed::int(t, row, "all_compilations")?
                    } else {
                        0
                    },
                };
                if matches!(class, Class::Album | Class::AlbumArtist) && rec.sync != 0 {
                    used.insert(rec.sync);
                }
                ensure!(
                    found.insert(pid, rec).is_none(),
                    "duplicate {} identity",
                    class.table()
                );
            }
            groups.insert(class, found);
        }
        let map = table(tables, "sort_map")?;
        let mut sort_keys = BTreeMap::new();
        for (row, n) in &map.rows {
            ensure!(*n == 1, "duplicate sort_map row");
            let name = text_cell(map, row, "name")?;
            // The library's empty name has the empty string as its key.
            let key = match map.at(row, "sort_key")? {
                crate::engine::verification::sql_tables::reader::Cell::Text(empty)
                    if empty.is_empty() =>
                {
                    None
                }
                _ => key_cell(map, row, "sort_key")?,
            };
            if let Some(key) = key {
                ensure!(
                    sort_keys.insert(name, key).is_none(),
                    "duplicate sort_map name"
                );
            }
        }
        Ok(Self {
            items,
            extras,
            by_sync,
            groups,
            sort_keys,
            used,
        })
    }
    pub(crate) fn item_by_sync(&self, sync_id: u64) -> Result<&ItemRec> {
        let sync = sync_id as i64;
        let pids = self
            .by_sync
            .get(&sync)
            .with_context(|| format!("track {sync_id:#018x} is not in the catalogue"))?;
        ensure!(
            pids.len() == 1,
            "track {sync_id:#018x} matches several catalogue items"
        );
        self.items
            .get(&pids[0])
            .with_context(|| format!("track {sync_id:#018x} has no item row"))
    }
}
