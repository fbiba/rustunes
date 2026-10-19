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
    pub(crate) fn groups(&self, class: Class) -> &BTreeMap<i64, GroupRec> {
        &self.groups[&class]
    }
    pub(crate) fn group(&self, class: Class, pid: i64) -> Option<&GroupRec> {
        self.groups[&class].get(&pid)
    }
    /// The receiver's grouping key of `name`, when `sort_map` knows the name.
    fn key_of(&self, name: &str) -> Option<&[u8]> {
        grouping_key(self.sort_keys.get(name)?)
    }
    fn group_name(&self, class: Class, pid: i64) -> &str {
        self.group(class, pid).map_or("", |g| g.name.as_str())
    }
}

/// One validated `update_track`, decomposed. `album_artist` is `Some` exactly
/// for the full set, which then also carries `artist` and `album`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Edit {
    pub sync_id: u64,
    pub item_pid: i64,
    pub keys: Vec<String>,
    pub title: Option<(String, String)>,
    pub genre: Option<String>,
    pub composer: Option<(String, String)>,
    pub track_number: Option<u16>,
    pub track_count: Option<u16>,
    pub disc_number: Option<u16>,
    pub disc_count: Option<u16>,
    pub year: Option<u16>,
    pub is_compilation: Option<bool>,
    pub content_rating: Option<u8>,
    pub copyright: Option<String>,
    pub comment: Option<String>,
    pub artist: Option<(String, String)>,
    pub album: Option<AlbumSend>,
    pub album_artist: Option<AlbumArtistSend>,
    /// Native-style `album_pid` alone: the track's current album, no text.
    pub album_noop: Option<u64>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AlbumSend {
    pub text: String,
    pub sort: String,
    pub pid: u64,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AlbumArtistSend {
    pub text: String,
    pub sort: String,
    pub pid: u64,
}

fn string<'a>(item: &'a BTreeMap<String, Value>, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_string)
}
fn pair(
    item: &BTreeMap<String, Value>,
    text: &str,
    sort: &str,
) -> Result<Option<(String, String)>> {
    match (string(item, text), string(item, sort)) {
        (Some(t), Some(s)) => Ok(Some((t.to_owned(), s.to_owned()))),
        (None, None) => Ok(None),
        _ => bail!("{text} and {sort} must be sent together"),
    }
}
fn number<T: TryFrom<u64>>(item: &BTreeMap<String, Value>, key: &str) -> Result<Option<T>> {
    item.get(key)
        .map(|value| {
            value
                .as_unsigned_integer()
                .and_then(|v| T::try_from(v).ok())
                .with_context(|| format!("{key} is out of range"))
        })
        .transpose()
}
fn wire(item: &BTreeMap<String, Value>, key: &str) -> Option<u64> {
    item.get(key)
        .and_then(Value::as_signed_integer)
        .map(|v| v as u64)
}

/// Syntactic decomposition of one update against a catalogue: the track must
/// exist and the key sets must be exactly those of a recipe operation.
pub(crate) fn parse_update(cat: &Catalogue, update: &TrackUpdate) -> Result<Edit> {
    let item = &update.item;
    let record = cat.item_by_sync(update.sync_id)?;
    let artist = pair(item, "artist", "sort_artist")?;
    let album = pair(item, "album", "sort_album")?;
    let album_artist = pair(item, "album_artist", "sort_album_artist")?;
    let album_pid = wire(item, "album_pid");
    let artist_pid = wire(item, "artist_pid");
    if album.is_some() {
        ensure!(
            album_pid.is_some(),
            "album text without album_pid does not regroup the track and leaves the catalogue inconsistent"
        );
    }
    let mut album_noop = None;
    if album.is_none() && album_pid.is_some() {
        album_noop = album_pid;
    }
    let full_album_artist = match album_artist {
        Some((text, sort)) => {
            ensure!(
                artist.is_some() && album.is_some() && artist_pid.is_some(),
                "album artist text needs the full set (artist, sort_artist, album, sort_album, album_pid, artist_pid)"
            );
            Some(AlbumArtistSend {
                text,
                sort,
                pid: artist_pid.context("artist_pid")?,
            })
        }
        None => {
            ensure!(
                artist_pid.is_none(),
                "artist_pid without album artist text re-keys the shared album artist"
            );
            None
        }
    };
    let album = album
        .map(|(text, sort)| -> Result<AlbumSend> {
            Ok(AlbumSend {
                text,
                sort,
                pid: album_pid.context("album_pid")?,
            })
        })
        .transpose()?;
    Ok(Edit {
        sync_id: update.sync_id,
        item_pid: record.pid,
        keys: item.keys().cloned().collect(),
        title: pair(item, "title", "sort_name")?,
        genre: string(item, "genre").map(str::to_owned),
        composer: pair(item, "composer", "sort_composer")?,
        track_number: number(item, "track_number")?,
        track_count: number(item, "track_count")?,
        disc_number: number(item, "disc_number")?,
        disc_count: number(item, "disc_count")?,
        year: number(item, "year")?,
        is_compilation: item.get("is_compilation").and_then(Value::as_boolean),
        content_rating: number(item, "content_rating")?,
        copyright: string(item, "copyright").map(str::to_owned),
        comment: string(item, "comment").map(str::to_owned),
        artist,
        album,
        album_artist: full_album_artist,
        album_noop,
    })
}

/// Identity of an album artist while a session is checked: a stored row, or
/// one the session itself creates (by its `sync_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AaRef {
    Existing(i64),
    New(i64),
}

struct SessionAlbum {
    text: String,
    aa: AaRef,
}
struct SessionAa {
    text: String,
}

/// State accumulated while the updates of one session are checked in order.
/// Every precondition reads the before catalogue; the only state later
/// updates see is the set of albums and album artists earlier updates create,
/// which they cannot join: two updates naming one new `album_pid` and
/// `artist_pid` in one session made two album rows and two album-artist rows
/// under those ids. Their names find those ids, so the later update waits.
pub(crate) struct Session<'a> {
    cat: &'a Catalogue,
    albums: BTreeMap<i64, SessionAlbum>,
    aas: BTreeMap<i64, SessionAa>,
    used: BTreeSet<i64>,
    leaving: BTreeSet<(Class, i64)>,
    names: Vec<(Class, String, Option<String>)>,
}

impl<'a> Session<'a> {
    pub(crate) fn new(cat: &'a Catalogue) -> Self {
        Self {
            cat,
            albums: BTreeMap::new(),
            aas: BTreeMap::new(),
            used: BTreeSet::new(),
            leaving: BTreeSet::new(),
            names: Vec::new(),
        }
    }

    fn id_free(&self, sync: i64) -> bool {
        !self.cat.used.contains(&sync) && !self.used.contains(&sync)
    }

    /// The `album_pid` an update must send to join the album named `text`
    /// under `aa`, if one exists in the catalogue or was created earlier in
    /// this session.
    fn find_album(&self, text: &str, aa: AaRef) -> Result<Option<u64>> {
        let mut found = Vec::new();
        if let AaRef::Existing(parent) = aa {
            for album in self.cat.groups(Class::Album).values() {
                if album.name == text && album.parent == parent {
                    ensure!(
                        album.sync != 0,
                        "album {text:?} by this album artist has no sync id and cannot be joined"
                    );
                    found.push(album.sync);
                }
            }
        }
        for (sync, album) in &self.albums {
            if album.text == text && album.aa == aa {
                found.push(*sync);
            }
        }
        ensure!(
            found.len() <= 1,
            "several albums named {text:?} exist for this album artist"
        );
        Ok(found.first().map(|sync| *sync as u64))
    }

    /// The `artist_pid` an update must send for an album artist named `text`.
    fn find_aa(&self, text: &str) -> Result<Option<(AaRef, u64)>> {
        let mut found = Vec::new();
        for (pid, aa) in self.cat.groups(Class::AlbumArtist) {
            if aa.name == text {
                ensure!(
                    aa.sync != 0,
                    "album artist {text:?} has no sync id and cannot be joined"
                );
                found.push((AaRef::Existing(*pid), aa.sync as u64));
            }
        }
        for (sync, aa) in &self.aas {
            if aa.text == text {
                found.push((AaRef::New(*sync), *sync as u64));
            }
        }
        ensure!(
            found.len() <= 1,
            "several album artists named {text:?} exist"
        );
        Ok(found.first().copied())
    }

    /// Joining by name must neither merge distinct stored groups nor rewrite a
    /// group's display name, which the receiver does when two names share a
    /// grouping key.
    fn hazard(&mut self, class: Class, text: &str, sort: Option<&str>) -> Result<()> {
        let cat = self.cat;
        let label = class.label();
        let groups = cat.groups(class);
        if let Some(key) = cat.key_of(text) {
            let hits: Vec<_> = groups
                .values()
                .filter(|g| g.key.as_deref() == Some(key))
                .collect();
            ensure!(
                hits.len() <= 1,
                "{label} {text:?} matches several existing {label} groups"
            );
            if let Some(group) = hits.first() {
                ensure!(
                    group.name == text,
                    "{label} {text:?} would merge into the existing {label} {:?} and rewrite its name",
                    group.name
                );
                if let Some(sort) = sort {
                    ensure!(
                        group.sort_effective() == sort,
                        "existing {label} {text:?} has sort name {:?}, not {sort:?}",
                        group.sort_effective()
                    );
                }
            }
        } else {
            for group in groups.values() {
                ensure!(
                    group.name == text || !may_merge(&group.name, text),
                    "{label} {text:?} differs only by case or punctuation from the existing {label} {:?}; the phone would merge them",
                    group.name
                );
            }
        }
        for (earlier_class, earlier, earlier_sort) in &self.names {
            if *earlier_class != class {
                continue;
            }
            ensure!(
                earlier == text || !may_merge(earlier, text),
                "{label} {text:?} differs only by case or punctuation from {earlier:?} in the same session"
            );
            if earlier == text
                && let (Some(a), Some(b)) = (earlier_sort, sort)
            {
                ensure!(
                    a == b,
                    "{label} {text:?} is sent with two different sort names in one session"
                );
            }
        }
        self.names
            .push((class, text.to_owned(), sort.map(str::to_owned)));
        Ok(())
    }

    fn leave(&mut self, class: Class, pid: i64) -> Result<()> {
        ensure!(
            self.leaving.insert((class, pid)),
            "two tracks would leave the same {} group in one session; needs separate sessions",
            class.label()
        );
        Ok(())
    }

    fn album(
        &mut self,
        send: &AlbumSend,
        aa: AaRef,
        year_changes: bool,
        compilation: bool,
    ) -> Result<()> {
        let cat = self.cat;
        let sync = send.pid as i64;
        let hits: Vec<_> = cat
            .groups(Class::Album)
            .values()
            .filter(|g| g.sync == sync)
            .collect();
        ensure!(
            hits.len() <= 1,
            "album_pid {:#018x} matches several albums",
            send.pid
        );
        if let Some(album) = hits.first() {
            ensure!(
                album.name == send.text
                    && album.sort_effective() == send.sort
                    && AaRef::Existing(album.parent) == aa,
                "album_pid {:#018x} belongs to album {:?} (sort {:?}) of another album artist or name; naming it differently would not rename it",
                send.pid,
                album.name,
                album.sort_effective()
            );
            ensure!(
                (album.all_compilations != 0) == compilation,
                "album {:?} and the track differ in compilation grouping; joining it is not qualified",
                album.name
            );
            ensure!(
                !year_changes,
                "a year change cannot be combined with joining an existing album; send the year in a separate session"
            );
            return Ok(());
        }
        ensure!(
            !self.albums.contains_key(&sync),
            "album_pid {:#018x} is created earlier in this session, and the phone would create it again under the same id; joining it waits for the next session",
            send.pid
        );
        ensure!(
            self.id_free(sync),
            "album_pid {:#018x} is already an album, album artist or track id",
            send.pid
        );
        ensure!(
            self.find_album(&send.text, aa)?.is_none(),
            "an album named {:?} by this album artist already exists; send its album_pid",
            send.text
        );
        ensure!(
            !compilation,
            "creating an album for a compilation track is not qualified"
        );
        self.used.insert(sync);
        self.albums.insert(
            sync,
            SessionAlbum {
                text: send.text.clone(),
                aa,
            },
        );
        Ok(())
    }

    fn album_artist(&mut self, send: &AlbumArtistSend) -> Result<AaRef> {
        let cat = self.cat;
        let sync = send.pid as i64;
        let hits: Vec<_> = cat
            .groups(Class::AlbumArtist)
            .iter()
            .filter(|(_, g)| g.sync == sync)
            .collect();
        ensure!(
            hits.len() <= 1,
            "artist_pid {:#018x} matches several album artists",
            send.pid
        );
        if let Some((pid, aa)) = hits.first() {
            ensure!(
                aa.name == send.text && aa.sort_effective() == send.sort,
                "artist_pid {:#018x} belongs to album artist {:?}; naming it differently would re-key or rename it",
                send.pid,
                aa.name
            );
            return Ok(AaRef::Existing(**pid));
        }
        ensure!(
            !self.aas.contains_key(&sync),
            "artist_pid {:#018x} is created earlier in this session, and the phone would create it again under the same id; joining it waits for the next session",
            send.pid
        );
        ensure!(
            self.id_free(sync),
            "artist_pid {:#018x} is already an album, album artist or track id",
            send.pid
        );
        ensure!(
            self.find_aa(&send.text)?.is_none(),
            "an album artist named {:?} already exists; send its artist_pid",
            send.text
        );
        self.used.insert(sync);
        self.aas.insert(
            sync,
            SessionAa {
                text: send.text.clone(),
            },
        );
        Ok(AaRef::New(sync))
    }

    /// Register one edit against the before catalogue, refusing it when it is
    /// outside the recipe.
    pub(crate) fn check(&mut self, e: &Edit) -> Result<()> {
        self.check_edit(e)
            .with_context(|| format!("track {:#018x}", e.sync_id))
    }

    fn check_edit(&mut self, e: &Edit) -> Result<()> {
        let cat = self.cat;
        let item = cat.items.get(&e.item_pid).context("item row missing")?;
        let extra = cat
            .extras
            .get(&e.item_pid)
            .context("track has no item_extra row")?;
        if let Some((title, sort)) = &e.title {
            ensure!(
                (title.as_str(), sort.as_str()) != (extra.title.as_str(), extra.sort_effective()),
                "title and sort name already have these values"
            );
        }
        let differs = |new: Option<i64>, current: i64, what: &str| -> Result<()> {
            if let Some(new) = new {
                ensure!(new != current, "{what} already is {current}");
            }
            Ok(())
        };
        differs(
            e.track_number.map(i64::from),
            item.track_number,
            "track_number",
        )?;
        differs(
            e.disc_number.map(i64::from),
            item.disc_number,
            "disc_number",
        )?;
        differs(
            e.track_count.map(i64::from),
            extra.track_count,
            "track_count",
        )?;
        differs(e.disc_count.map(i64::from), extra.disc_count, "disc_count")?;
        differs(e.year.map(i64::from), extra.year, "year")?;
        differs(
            e.content_rating.map(i64::from),
            extra.content_rating,
            "content_rating",
        )?;
        if let Some(flag) = e.is_compilation {
            ensure!(flag != item.compilation, "is_compilation already is {flag}");
        }
        if let Some(copyright) = &e.copyright {
            ensure!(
                *copyright != extra.copyright,
                "copyright already has this value"
            );
        }
        if let Some(comment) = &e.comment {
            ensure!(*comment != extra.comment, "comment already has this value");
        }
        if let Some(genre) = &e.genre {
            ensure!(
                genre != cat.group_name(Class::Genre, item.genre),
                "genre already is {genre:?}"
            );
            self.hazard(Class::Genre, genre, None)?;
        }
        if let Some((composer, sort)) = &e.composer {
            ensure!(
                composer != cat.group_name(Class::Composer, item.composer),
                "composer already is {composer:?}; changing only a composer's sort name is unsupported"
            );
            self.hazard(Class::Composer, composer, Some(sort))?;
        }
        let compilation = e.is_compilation.unwrap_or(item.compilation);
        let year_changes = e.year.is_some();
        let current_artist = cat.group(Class::ItemArtist, item.item_artist);
        let current_album = cat.group(Class::Album, item.album);
        let current_aa = cat.group(Class::AlbumArtist, item.album_artist);
        if let Some(full) = &e.album_artist {
            let (artist, album) = (
                e.artist.as_ref().context("full set lacks artist")?,
                e.album.as_ref().context("full set lacks album")?,
            );
            let current_artist = current_artist.context(
                "the track has no artist group, so an album artist change is unsupported",
            )?;
            let current_album = current_album.context(
                "the track has no album group, so an album artist change is unsupported",
            )?;
            let current_aa = current_aa.context(
                "the track has no album artist group, so an album artist change is unsupported",
            )?;
            ensure!(
                full.text != current_aa.name,
                "album artist already is {:?}; changing only an album artist's sort name is unsupported",
                full.text
            );
            ensure!(
                current_artist.rep == item.pid,
                "the track is not the stored representative of its artist group; an album artist change needs a multi-session sequence"
            );
            ensure!(
                current_aa.rep == item.pid,
                "the track is not the stored representative of its album artist group; an album artist change needs a multi-session sequence"
            );
            if artist.0 == current_artist.name {
                ensure!(
                    artist.1 == current_artist.sort_effective(),
                    "the artist sort name cannot change together with an album artist change"
                );
                if let Some(key) = cat.key_of(&current_artist.name) {
                    ensure!(
                        current_artist.key.as_deref() == Some(key),
                        "the track's artist group {:?} has a stale grouping key; the receiver would move the track",
                        current_artist.name
                    );
                }
            } else {
                self.hazard(Class::ItemArtist, &artist.0, Some(&artist.1))?;
                self.leave(Class::ItemArtist, item.item_artist)?;
            }
            if album.text == current_album.name {
                ensure!(
                    album.sort == current_album.sort_effective(),
                    "the album sort name cannot change together with an album artist change"
                );
            }
            self.hazard(Class::AlbumArtist, &full.text, Some(&full.sort))?;
            self.leave(Class::AlbumArtist, item.album_artist)?;
            let aa = self.album_artist(full)?;
            self.album(album, aa, year_changes, compilation)?;
        } else {
            if let Some(artist) = &e.artist {
                let group = current_artist.context("the track has no artist group")?;
                ensure!(
                    artist.0 != group.name,
                    "artist already is {:?}; changing only an artist's sort name is unsupported",
                    artist.0
                );
                ensure!(
                    group.rep == item.pid,
                    "the track is not the stored representative of its artist group {:?}; the receiver would rename the shared group for every member",
                    group.name
                );
                self.hazard(Class::ItemArtist, &artist.0, Some(&artist.1))?;
                self.leave(Class::ItemArtist, item.item_artist)?;
            }
            if let Some(album) = &e.album {
                ensure!(
                    item.album_artist != 0 && current_aa.is_some(),
                    "the track has no album artist, so it cannot move to another album"
                );
                ensure!(
                    current_album.is_none_or(|g| g.name != album.text),
                    "album already is {:?}; changing only an album's sort name is unsupported",
                    album.text
                );
                self.album(
                    album,
                    AaRef::Existing(item.album_artist),
                    year_changes,
                    compilation,
                )?;
            }
            if let Some(pid) = e.album_noop {
                let album = current_album.context("the track has no album")?;
                ensure!(
                    album.sync != 0 && pid as i64 == album.sync,
                    "album_pid {pid:#018x} without a name would re-key the track's album, which is not its current album id"
                );
            }
        }
        Ok(())
    }
}

/// Parse and check a whole session against the before catalogue.
pub(crate) fn conform(before: &Tables, updates: &[TrackUpdate]) -> Result<Vec<Edit>> {
    conform_on(&Catalogue::read(before)?, updates)
}

/// `conform` against an already read (or simulated) catalogue.
pub(crate) fn conform_on(cat: &Catalogue, updates: &[TrackUpdate]) -> Result<Vec<Edit>> {
    validate(updates)?;
    let mut session = Session::new(cat);
    let mut edits = Vec::new();
    for update in updates {
        let edit =
            parse_update(cat, update).with_context(|| format!("track {:#018x}", update.sync_id))?;
        session.check(&edit)?;
        edits.push(edit);
    }
    Ok(edits)
}

/// Accepts exactly the operations `plan` can produce, plus the native-style
/// no-op `album_pid` equal to the track's current album id without a name.
pub(crate) fn check_conformant(before: &Tables, updates: &[TrackUpdate]) -> Result<()> {
    conform(before, updates).map(drop)
}

fn check_text(what: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty(),
        "{what}: empty strings are refused; clearing a field is unsupported"
    );
    ensure!(value.len() <= 4096, "{what} exceeds 4096 bytes");
    ensure!(
        !value.chars().any(char::is_control),
        "{what} contains a control character"
    );
    Ok(())
}
fn check_texts(request: &EditRequest) -> Result<()> {
    for (what, value) in [
        ("title", &request.title),
        ("sort_title", &request.sort_title),
        ("artist", &request.artist),
        ("sort_artist", &request.sort_artist),
        ("album", &request.album),
        ("sort_album", &request.sort_album),
        ("album_artist", &request.album_artist),
        ("sort_album_artist", &request.sort_album_artist),
        ("genre", &request.genre),
        ("composer", &request.composer),
        ("sort_composer", &request.sort_composer),
        ("copyright", &request.copyright),
    ] {
        if let Some(value) = value
            && !(what == "copyright" && value.is_empty())
        {
            check_text(what, value)?;
        }
    }
    if let Some(comment) = &request.comment
        && !comment.is_empty()
    {
        music_protocol::media::metadata::comment_text(&[comment.as_str()])?;
    }
    Ok(())
}

/// The pair to send when a name and/or sort value is requested, or `None`
/// when nothing differs from the current pair.
fn pair_change(
    what: &str,
    current: (&str, &str),
    text: Option<&String>,
    sort: Option<&String>,
    sort_only_allowed: bool,
) -> Result<Option<(String, String)>> {
    let (new_text, new_sort) = match (text, sort) {
        (None, None) => return Ok(None),
        (Some(text), sort) => {
            let sort = sort.cloned().unwrap_or_else(|| {
                if text == current.0 {
                    current.1.to_owned()
                } else {
                    text.clone()
                }
            });
            (text.clone(), sort)
        }
        (None, Some(sort)) => (current.0.to_owned(), sort.clone()),
    };
    if (new_text.as_str(), new_sort.as_str()) == current {
        return Ok(None);
    }
    ensure!(
        !new_text.is_empty(),
        "{what} has no current name, so a sort value alone cannot be sent"
    );
    if new_text == current.0 {
        ensure!(
            sort_only_allowed,
            "{what}: changing only the sort value of a shared name is unsupported"
        );
    }
    Ok(Some((new_text, new_sort)))
}

fn put_text(item: &mut BTreeMap<String, Value>, key: &str, value: &str) {
    item.insert(key.to_owned(), Value::String(value.to_owned()));
}
fn put_int(item: &mut BTreeMap<String, Value>, key: &str, value: i64) {
    item.insert(key.to_owned(), Value::Integer(value.into()));
}

struct Draw<'a, 'f> {
    cat: &'a Catalogue,
    fresh: &'f mut dyn FnMut() -> u64,
    reserved: BTreeSet<i64>,
}
impl Draw<'_, '_> {
    fn next(&mut self, session: &Session<'_>) -> Result<u64> {
        for _ in 0..1024 {
            let id = (self.fresh)();
            let sync = id as i64;
            if id == 0
                || self.cat.used.contains(&sync)
                || self.reserved.contains(&sync)
                || session.used.contains(&sync)
            {
                continue;
            }
            self.reserved.insert(sync);
            return Ok(id);
        }
        bail!("the fresh id generator produced no usable id")
    }
}

/// Build the recipe operations for `requests` against the before catalogue.
/// `fresh` supplies candidate random 64-bit ids; an id is used only when it is
/// nonzero and not an existing album, album artist or track id nor one already
/// used in this plan. Requests without a remaining change are dropped.
pub(crate) fn plan(
    before: &Tables,
    requests: &[EditRequest],
    fresh: &mut dyn FnMut() -> u64,
) -> Result<Vec<TrackUpdate>> {
    plan_on(&Catalogue::read(before)?, requests, fresh)
}

/// `plan` against an already read (or simulated) catalogue.
pub(crate) fn plan_on(
    cat: &Catalogue,
    requests: &[EditRequest],
    fresh: &mut dyn FnMut() -> u64,
) -> Result<Vec<TrackUpdate>> {
    let mut session = Session::new(cat);
    let mut draw = Draw {
        cat,
        fresh,
        reserved: BTreeSet::new(),
    };
    let mut seen = BTreeSet::new();
    let mut updates = Vec::new();
    for request in requests {
        ensure!(request.track != 0, "edit request names track zero");
        ensure!(
            seen.insert(request.track),
            "track {:#018x} appears in more than one request",
            request.track
        );
        let update = plan_one(cat, &mut session, &mut draw, request)
            .with_context(|| format!("track {:#018x}", request.track))?;
        if let Some(update) = update {
            let edit = parse_update(cat, &update)
                .with_context(|| format!("track {:#018x}", request.track))?;
            session.check(&edit)?;
            updates.push(update);
        }
    }
    if !updates.is_empty() {
        validate(&updates)?;
    }
    Ok(updates)
}

fn plan_one(
    cat: &Catalogue,
    session: &mut Session<'_>,
    draw: &mut Draw<'_, '_>,
    r: &EditRequest,
) -> Result<Option<TrackUpdate>> {
    check_texts(r)?;
    let item = cat.item_by_sync(r.track)?;
    let extra = cat
        .extras
        .get(&item.pid)
        .context("track has no item_extra row")?;
    let mut out: BTreeMap<String, Value> = BTreeMap::new();

    if let Some((title, sort)) = pair_change(
        "title",
        (&extra.title, extra.sort_effective()),
        r.title.as_ref(),
        r.sort_title.as_ref(),
        true,
    )? {
        put_text(&mut out, "title", &title);
        put_text(&mut out, "sort_name", &sort);
    }
    if let Some(genre) = &r.genre
        && genre != cat.group_name(Class::Genre, item.genre)
    {
        put_text(&mut out, "genre", genre);
    }
    let composer = cat.group(Class::Composer, item.composer);
    if let Some((composer, sort)) = pair_change(
        "composer",
        (
            composer.map_or("", |g| g.name.as_str()),
            composer.map_or("", |g| g.sort_effective()),
        ),
        r.composer.as_ref(),
        r.sort_composer.as_ref(),
        false,
    )? {
        put_text(&mut out, "composer", &composer);
        put_text(&mut out, "sort_composer", &sort);
    }
    for (key, requested, current) in [
        ("year", r.year.map(i64::from), extra.year),
        (
            "track_number",
            r.track_number.map(i64::from),
            item.track_number,
        ),
        (
            "track_count",
            r.track_count.map(i64::from),
            extra.track_count,
        ),
        (
            "disc_number",
            r.disc_number.map(i64::from),
            item.disc_number,
        ),
        ("disc_count", r.disc_count.map(i64::from), extra.disc_count),
        (
            "content_rating",
            r.content_rating.map(i64::from),
            extra.content_rating,
        ),
    ] {
        if let Some(value) = requested
            && value != current
        {
            put_int(&mut out, key, value);
        }
    }
    if let Some(flag) = r.is_compilation
        && flag != item.compilation
    {
        out.insert("is_compilation".to_owned(), Value::Boolean(flag));
    }
    if let Some(copyright) = &r.copyright
        && *copyright != extra.copyright
    {
        put_text(&mut out, "copyright", copyright);
    }
    if let Some(comment) = &r.comment
        && *comment != extra.comment
    {
        put_text(&mut out, "comment", comment);
    }

    let artist = cat.group(Class::ItemArtist, item.item_artist);
    let album = cat.group(Class::Album, item.album);
    let aa = cat.group(Class::AlbumArtist, item.album_artist);
    let current_artist = (
        artist.map_or("", |g| g.name.as_str()),
        artist.map_or("", |g| g.sort_effective()),
    );
    let current_album = (
        album.map_or("", |g| g.name.as_str()),
        album.map_or("", |g| g.sort_effective()),
    );
    let current_aa = (
        aa.map_or("", |g| g.name.as_str()),
        aa.map_or("", |g| g.sort_effective()),
    );
    let artist_change = pair_change(
        "artist",
        current_artist,
        r.artist.as_ref(),
        r.sort_artist.as_ref(),
        false,
    )?;
    let album_change = pair_change(
        "album",
        current_album,
        r.album.as_ref(),
        r.sort_album.as_ref(),
        false,
    )?;
    let aa_change = pair_change(
        "album artist",
        current_aa,
        r.album_artist.as_ref(),
        r.sort_album_artist.as_ref(),
        false,
    )?;

    if let Some((aa_text, aa_sort)) = aa_change {
        let (artist_text, artist_sort) = artist_change
            .unwrap_or_else(|| (current_artist.0.to_owned(), current_artist.1.to_owned()));
        let (album_text, album_sort) = album_change
            .unwrap_or_else(|| (current_album.0.to_owned(), current_album.1.to_owned()));
        ensure!(
            !artist_text.is_empty() && !album_text.is_empty(),
            "an album artist change needs the track's current artist and album"
        );
        let (album_pid, artist_pid) = match session.find_aa(&aa_text)? {
            Some((aa_ref, sync)) => {
                let album_pid = match session.find_album(&album_text, aa_ref)? {
                    Some(pid) => pid,
                    None => draw.next(session)?,
                };
                (album_pid, sync)
            }
            None => {
                let album_pid = draw.next(session)?;
                (album_pid, draw.next(session)?)
            }
        };
        put_text(&mut out, "artist", &artist_text);
        put_text(&mut out, "sort_artist", &artist_sort);
        put_text(&mut out, "album", &album_text);
        put_text(&mut out, "sort_album", &album_sort);
        put_text(&mut out, "album_artist", &aa_text);
        put_text(&mut out, "sort_album_artist", &aa_sort);
        put_int(&mut out, "album_pid", album_pid as i64);
        put_int(&mut out, "artist_pid", artist_pid as i64);
    } else {
        if let Some((text, sort)) = artist_change {
            put_text(&mut out, "artist", &text);
            put_text(&mut out, "sort_artist", &sort);
        }
        if let Some((text, sort)) = album_change {
            let pid = match session.find_album(&text, AaRef::Existing(item.album_artist))? {
                Some(pid) => pid,
                None => draw.next(session)?,
            };
            put_text(&mut out, "album", &text);
            put_text(&mut out, "sort_album", &sort);
            put_int(&mut out, "album_pid", pid as i64);
        }
    }
    Ok((!out.is_empty()).then_some(TrackUpdate {
        sync_id: r.track,
        item: out,
    }))
}

/// The group row of `class` an item is filed under.
fn member_of(item: &ItemRec, class: Class) -> i64 {
    match class {
        Class::ItemArtist => item.item_artist,
        Class::AlbumArtist => item.album_artist,
        Class::Album => item.album,
        Class::Composer => item.composer,
        Class::Genre => item.genre,
    }
}
fn file_under(item: &mut ItemRec, class: Class, group: i64) {
    match class {
        Class::ItemArtist => item.item_artist = group,
        Class::AlbumArtist => item.album_artist = group,
        Class::Album => item.album = group,
        Class::Composer => item.composer = group,
        Class::Genre => item.genre = group,
    }
}

impl Catalogue {
    /// Items filed under a group, without `exclude`.
    fn members(&self, class: Class, group: i64, exclude: i64) -> BTreeSet<i64> {
        self.items
            .values()
            .filter(|i| i.pid != exclude && member_of(i, class) == group)
            .map(|i| i.pid)
            .collect()
    }

    /// A group pid below every stored one, for a row this simulation creates.
    fn synthetic(&self, class: Class) -> i64 {
        self.groups[&class]
            .keys()
            .next()
            .map_or(-1, |lowest| (*lowest).min(0) - 1)
    }

    fn create(
        &mut self,
        class: Class,
        name: &str,
        sort: Option<&str>,
        sync: i64,
        parent: i64,
        item: i64,
    ) -> i64 {
        let pid = self.synthetic(class);
        let key = self.key_of(name).map(<[u8]>::to_vec);
        self.groups
            .get_mut(&class)
            .expect("every class is read")
            .insert(
                pid,
                GroupRec {
                    name: name.to_owned(),
                    sort: sort.map(str::to_owned),
                    rep: item,
                    key,
                    sync,
                    parent,
                    all_compilations: 0,
                },
            );
        if sync != 0 {
            self.used.insert(sync);
        }
        pid
    }

    /// The row an artist, composer or genre name joins, by grouping key; a
    /// name the library does not know yet joins a row this simulation created
    /// under that exact name. `None`: the receiver creates a row.
    fn by_key(&self, class: Class, name: &str) -> Option<i64> {
        let key = self.key_of(name);
        self.groups[&class]
            .iter()
            .find(|(_, g)| match key {
                Some(key) => g.key.as_deref() == Some(key),
                None => g.key.is_none() && g.name == name,
            })
            .map(|(pid, _)| *pid)
    }

    fn by_sync(&self, class: Class, sync: i64) -> Option<i64> {
        self.groups[&class]
            .iter()
            .find(|(_, g)| g.sync == sync)
            .map(|(pid, _)| *pid)
    }

    /// Move `item` from its row of `class` to `new`, with the verifier's
    /// representative bookkeeping: a leaving representative hands over to the
    /// lowest remaining member (or zero), and a joiner becomes the
    /// representative of an empty row it did not create.
    fn relocate(&mut self, class: Class, item: i64, new: i64, created: bool) {
        let old = member_of(&self.items[&item], class);
        if old == new {
            return;
        }
        if old != 0 && self.groups[&class].get(&old).is_some_and(|g| g.rep == item) {
            let next = self.members(class, old, item).first().copied().unwrap_or(0);
            if let Some(group) = self.groups.get_mut(&class).and_then(|g| g.get_mut(&old)) {
                group.rep = next;
            }
        }
        if new != 0 && !created && self.members(class, new, item).is_empty() {
            if let Some(group) = self.groups.get_mut(&class).and_then(|g| g.get_mut(&new)) {
                group.rep = item;
            }
        }
        if let Some(record) = self.items.get_mut(&item) {
            file_under(record, class, new);
        }
    }

    /// Join or create the row a name-matched group edit sends the item to.
    fn move_by_key(
        &mut self,
        class: Class,
        item: i64,
        name: &str,
        sort: Option<&str>,
        created: &mut Vec<(Class, i64)>,
    ) {
        let (group, new) = match self.by_key(class, name) {
            Some(group) => (group, false),
            None => (self.create(class, name, sort, 0, 0, item), true),
        };
        if new {
            created.push((class, group));
        }
        self.relocate(class, item, group, new);
    }

    /// Join or create the album or album artist a sync id names.
    #[allow(clippy::too_many_arguments)]
    fn move_by_sync(
        &mut self,
        class: Class,
        item: i64,
        sync: i64,
        name: &str,
        sort: &str,
        parent: i64,
        created: &mut Vec<(Class, i64)>,
    ) -> i64 {
        let (group, new) = match self.by_sync(class, sync) {
            Some(group) => (group, false),
            None => (
                self.create(class, name, Some(sort), sync, parent, item),
                true,
            ),
        };
        if new {
            created.push((class, group));
        }
        self.relocate(class, item, group, new);
        group
    }

    /// The catalogue after one session of `edits`, as the verifier models it
    /// (`verification::metadata_edit_sql`): fields change, groups are joined
    /// or created, representatives move as `relocate` says, and a row created
    /// in the session ends with its lowest member. Rows created here get pids
    /// below every stored one; names new to the library have no grouping key.
    pub(crate) fn simulate(&mut self, edits: &[Edit]) -> Result<()> {
        let mut created = Vec::new();
        for e in edits {
            let pid = e.item_pid;
            ensure!(
                self.items.contains_key(&pid),
                "track {:#018x} has no item row",
                e.sync_id
            );
            {
                let item = self.items.get_mut(&pid).context("item row")?;
                if let Some(n) = e.track_number {
                    item.track_number = n.into();
                }
                if let Some(n) = e.disc_number {
                    item.disc_number = n.into();
                }
                if let Some(flag) = e.is_compilation {
                    item.compilation = flag;
                }
            }
            {
                let extra = self.extras.get_mut(&pid).context("item_extra row")?;
                if let Some((title, sort)) = &e.title {
                    extra.title = title.clone();
                    extra.sort_title = sort.clone();
                }
                if let Some(n) = e.track_count {
                    extra.track_count = n.into();
                }
                if let Some(n) = e.disc_count {
                    extra.disc_count = n.into();
                }
                if let Some(n) = e.year {
                    extra.year = n.into();
                }
                if let Some(n) = e.content_rating {
                    extra.content_rating = n.into();
                }
                if let Some(copyright) = &e.copyright {
                    extra.copyright = copyright.clone();
                }
                if let Some(comment) = &e.comment {
                    extra.comment = comment.clone();
                }
            }
            let mut parent = self.items[&pid].album_artist;
            if let Some(full) = &e.album_artist {
                parent = self.move_by_sync(
                    Class::AlbumArtist,
                    pid,
                    full.pid as i64,
                    &full.text,
                    &full.sort,
                    0,
                    &mut created,
                );
            }
            if let Some(album) = &e.album {
                self.move_by_sync(
                    Class::Album,
                    pid,
                    album.pid as i64,
                    &album.text,
                    &album.sort,
                    parent,
                    &mut created,
                );
            }
            if let Some((artist, sort)) = &e.artist {
                self.move_by_key(Class::ItemArtist, pid, artist, Some(sort), &mut created);
            }
            if let Some((composer, sort)) = &e.composer {
                self.move_by_key(Class::Composer, pid, composer, Some(sort), &mut created);
            }
            if let Some(genre) = &e.genre {
                self.move_by_key(Class::Genre, pid, genre, None, &mut created);
            }
        }
        for (class, group) in created {
            if let Some(lowest) = self.members(class, group, 0).first().copied()
                && let Some(row) = self.groups.get_mut(&class).and_then(|g| g.get_mut(&group))
            {
                row.rep = lowest;
            }
        }
        Ok(())
    }

    /// Spell each group name of `request` as the phone already does when a
    /// group differs from it only in ASCII case: the phone files both under
    /// one group and would rename it to the sent spelling, which the recipe
    /// refuses, while sending the stored spelling joins it unchanged.
    pub(crate) fn respell(&self, request: &mut EditRequest) -> Result<()> {
        let item = self.item_by_sync(request.track)?.clone();
        let stored = |class: Class, text: &str, parent: Option<i64>| -> Option<String> {
            let mut names: BTreeSet<&str> = self.groups[&class]
                .values()
                .filter(|g| parent.is_none_or(|p| g.parent == p))
                .filter(|g| {
                    g.name != text
                        && text.is_ascii()
                        && g.name.is_ascii()
                        && g.name.eq_ignore_ascii_case(text)
                })
                .map(|g| g.name.as_str())
                .collect();
            let exact = self.groups[&class]
                .values()
                .any(|g| parent.is_none_or(|p| g.parent == p) && g.name == text);
            (names.len() == 1 && !exact).then(|| names.pop_first().expect("one name").to_owned())
        };
        for (class, field) in [
            (Class::ItemArtist, &mut request.artist),
            (Class::AlbumArtist, &mut request.album_artist),
            (Class::Composer, &mut request.composer),
            (Class::Genre, &mut request.genre),
        ] {
            if let Some(text) = field.as_ref()
                && let Some(spelling) = stored(class, text, None)
            {
                *field = Some(spelling);
            }
        }
        let parent = match &request.album_artist {
            Some(text) => self
                .groups(Class::AlbumArtist)
                .iter()
                .find(|(_, g)| g.name == *text)
                .map(|(pid, _)| *pid),
            None => Some(item.album_artist),
        };
        if let (Some(parent), Some(text)) = (parent, request.album.as_ref())
            && let Some(spelling) = stored(Class::Album, text, Some(parent))
        {
            request.album = Some(spelling);
        }
        Ok(())
    }
}

#[cfg(test)]
impl Catalogue {
    /// Every group row and each item's fields and groups, by name rather than
    /// pid, so a simulated catalogue compares with an observed one.
    pub(crate) fn view(&self) -> Vec<String> {
        let name = |class: Class, pid: i64| self.group_name(class, pid).to_owned();
        let mut rows = Vec::new();
        for class in Class::ALL {
            for group in self.groups[&class].values() {
                rows.push(format!(
                    "{} {:?} sort {:?} sync {} parent {:?} representative {}",
                    class.label(),
                    group.name,
                    group.sort_effective(),
                    group.sync,
                    name(Class::AlbumArtist, group.parent),
                    group.rep
                ));
            }
        }
        for item in self.items.values() {
            let extra = &self.extras[&item.pid];
            rows.push(format!(
                "item {} {:?}/{:?} track {}/{} disc {}/{} year {} rating {} compilation {} copyright {:?} comment {:?} groups {:?}",
                item.pid,
                extra.title,
                extra.sort_effective(),
                item.track_number,
                extra.track_count,
                item.disc_number,
                extra.disc_count,
                extra.year,
                extra.content_rating,
                item.compilation,
                extra.copyright,
                extra.comment,
                Class::ALL.map(|class| name(class, member_of(item, class))),
            ));
        }
        rows.sort();
        rows
    }
}

/// Whether `request` still has something to send against `cat`; a request
/// the recipe refuses there counts as pending.
fn pending(cat: &Catalogue, request: &EditRequest, fresh: &mut dyn FnMut() -> u64) -> bool {
    plan_on(cat, std::slice::from_ref(request), fresh).map_or(true, |updates| !updates.is_empty())
}

/// A session's `update_track` operations: one operation file holds them
/// with its `update_db_info`.
const SESSION_UPDATES: usize = music_protocol::protocol::operations::MAX_OPERATIONS - 1;

/// What one session against `cat` can carry: requests in order, each kept
/// when it plans together with those kept before it, within
/// [`SESSION_UPDATES`]. A request that cannot go whole sends what can go now
/// (without its year, its group moves, or both), and the rest waits for a
/// later session, as does a request a full session leaves out. Requests
/// nothing of which can go wait, with the reason each refused alone.
pub(crate) fn pack(
    cat: &Catalogue,
    requests: &[EditRequest],
    fresh: &mut dyn FnMut() -> u64,
) -> (Vec<EditRequest>, Vec<(u64, String)>) {
    pack_within(cat, requests, SESSION_UPDATES, fresh)
}

/// [`pack`] with at most `most` updates in the session.
fn pack_within(
    cat: &Catalogue,
    requests: &[EditRequest],
    most: usize,
    fresh: &mut dyn FnMut() -> u64,
) -> (Vec<EditRequest>, Vec<(u64, String)>) {
    let mut kept: Vec<EditRequest> = Vec::new();
    let mut waiting = Vec::new();
    for request in requests {
        if !pending(cat, request, fresh) {
            continue;
        }
        let fits = |kept: &[EditRequest],
                    candidate: &EditRequest,
                    fresh: &mut dyn FnMut() -> u64| {
            let mut trial = kept.to_vec();
            trial.push(candidate.clone());
            plan_on(cat, &trial, fresh).map(|updates| !updates.is_empty() && updates.len() <= most)
        };
        match fits(&kept, request, fresh) {
            Ok(true) => kept.push(request.clone()),
            // Too many for any session, not only for a full one.
            Ok(false) if kept.is_empty() => {
                if let Ok(updates) = plan_on(cat, std::slice::from_ref(request), fresh)
                    && updates.len() > most
                {
                    waiting.push((
                        request.track,
                        format!(
                            "it needs {} track updates, more than the {most} one session holds",
                            updates.len()
                        ),
                    ));
                }
            }
            Ok(false) => {}
            Err(error) => {
                // What can go now goes; the rest waits for a later session.
                let yearless = EditRequest {
                    year: None,
                    ..request.clone()
                };
                let ungrouped = EditRequest {
                    artist: None,
                    sort_artist: None,
                    album: None,
                    sort_album: None,
                    album_artist: None,
                    sort_album_artist: None,
                    ..request.clone()
                };
                let neither = EditRequest {
                    year: None,
                    ..ungrouped.clone()
                };
                if let Some(part) = [yearless, ungrouped, neither]
                    .into_iter()
                    .find(|part| matches!(fits(&kept, part, fresh), Ok(true)))
                {
                    kept.push(part);
                    continue;
                }
                let alone = plan_on(cat, std::slice::from_ref(request), fresh)
                    .err()
                    .unwrap_or(error);
                waiting.push((request.track, format!("{alone:#}")));
            }
        }
    }
    (kept, waiting)
}

/// The sessions that carry `requests` to completion from `before`: each is
/// what `pack` keeps against the catalogue the earlier sessions leave, as
/// `Catalogue::simulate` models them. Refuses, naming each request and its
/// reason, when no session can carry the rest, or after `limit` sessions.
pub(crate) fn schedule(
    before: &Tables,
    requests: &[EditRequest],
    limit: usize,
    fresh: &mut dyn FnMut() -> u64,
) -> Result<Vec<Vec<u64>>> {
    schedule_within(before, requests, limit, SESSION_UPDATES, fresh)
}

/// [`schedule`] with at most `most` updates per session.
fn schedule_within(
    before: &Tables,
    requests: &[EditRequest],
    limit: usize,
    most: usize,
    fresh: &mut dyn FnMut() -> u64,
) -> Result<Vec<Vec<u64>>> {
    let mut cat = Catalogue::read(before)?;
    let mut sessions = Vec::new();
    loop {
        let (kept, waiting) = pack_within(&cat, requests, most, fresh);
        if kept.is_empty() {
            ensure!(
                waiting.is_empty(),
                "{} metadata update(s) cannot be sent{}:\n{}",
                waiting.len(),
                if sessions.is_empty() {
                    String::new()
                } else {
                    format!(" after {} session(s)", sessions.len())
                },
                waiting
                    .iter()
                    .map(|(track, reason)| format!("  {track:016x}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            return Ok(sessions);
        }
        ensure!(
            sessions.len() < limit,
            "metadata updates need more than {limit} sessions"
        );
        let updates = plan_on(&cat, &kept, fresh)?;
        let edits = conform_on(&cat, &updates)?;
        cat.simulate(&edits)?;
        sessions.push(updates.iter().map(|u| u.sync_id).collect());
    }
}

#[cfg(test)]
pub(crate) mod tests;
