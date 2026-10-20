//! Full typed SQL validation for ordinary new-group ALAC recovery.
//! No journal, protocol, or physical authority is constructed here.
use crate::engine::verification::sql_values as typed;
use crate::{
    engine::execution::insert,
    engine::execution::native_insert::Control,
    engine::planning::collections::{CandidateIds, Planner},
    engine::recovery::retained::playback::Window,
    engine::state::journal::{AttemptKind, PreparedAttempt},
    engine::verification::sql_tables::{Capture, reader},
};
use anyhow::{Context, Result, ensure};
use music_protocol::catalogue::sqlite::Catalog;
use music_protocol::media::metadata::Metadata;
use reader::{Budget, Cell, Row, Snapshot, Table, hex};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use typed::*;

mod rows;

/// A well-formed effect that needs an independent immediate-to-current proof.
/// This error is never completion authority. Input, schema, I/O and control
/// failures do not acquire this classification.
#[derive(Debug)]
pub(super) struct PlaybackEffectMismatch(pub(super) &'static str);
impl std::fmt::Display for PlaybackEffectMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ordinary insertion has later {} changes", self.0)
    }
}
impl std::error::Error for PlaybackEffectMismatch {}

const TARGET_TABLES: [&str; 10] = [
    "chapter",
    "item",
    "item_extra",
    "item_playback",
    "item_search",
    "item_state",
    "item_stats",
    "item_store",
    "item_video",
    "lyrics",
];
#[derive(Clone, Debug, Serialize)]
struct Group {
    table: &'static str,
    key: &'static str,
    label: String,
    sort_label: String,
    wire: i64,
    class: i64,
}
struct BeforeFacts {
    slot: i64,
    tombstone: Option<Row>,
    maximum: i64,
    generation: i64,
    base: i64,
    groups: Vec<Group>,
}
struct Wire {
    title: String,
    album: String,
    artist: String,
    album_artist: String,
    genre: Option<String>,
    copyright: Option<String>,
    year: i64,
    track: i64,
    tracks: i64,
    disc: i64,
    discs: i64,
    millis: i64,
    ticks: i64,
    timestamp: f64,
    database: i64,
    primary: i64,
    album_wire: i64,
    artist_wire: i64,
}
#[derive(Clone, Debug, Serialize)]
struct Map {
    name: String,
    order: i64,
    section: i64,
    sort_key: Vec<u8>,
}
struct Observed {
    new_maps: Vec<Map>,
    dates: [f64; 2],
    integrity: Vec<u8>,
    recent_resets: Vec<serde_json::Value>,
    playback: Option<serde_json::Value>,
}
/// Whether the full SQL model covers this insertion: an artless one that
/// creates its album and artist, with no composer, compilation or rating.
/// Recovery proves any other by the catalogue verifier, as the insertion
/// itself was checked.
pub(super) fn covers(p: &PreparedAttempt, source: &Metadata) -> Result<bool> {
    let scalar = source.projected()?;
    Ok(p.kind == AttemptKind::Insert
        && p.content.artwork.is_none()
        && p.new_album_pid.is_some()
        && p.new_artist_pid.is_some()
        && scalar.composer.is_none()
        && scalar.compilation != Some(true)
        && scalar.content_rating.is_none_or(|n| n == 0))
}

impl Wire {
    fn read(p: &PreparedAttempt, source: &Metadata, before: &Catalog) -> Result<Self> {
        ensure!(
            p.kind == AttemptKind::Insert && p.content.artwork.is_none(),
            "ordinary SQL requires an artless insertion"
        );
        let album = p
            .new_album_pid
            .context("ordinary reused-album SQL profile not yet qualified")?;
        let artist = p
            .new_artist_pid
            .context("ordinary reused-artist SQL profile not yet qualified")?;
        let scalar = source.projected()?;
        ensure!(
            scalar.composer.is_none()
                && scalar.compilation != Some(true)
                && scalar.content_rating.is_none_or(|n| n == 0),
            "ordinary composer/compilation/rating SQL branch not yet qualified"
        );
        let mut planner = Planner::new(before, &BTreeSet::new());
        let planned = planner.plan(
            source,
            CandidateIds {
                track: p.content.track_pid,
                album: Some(album),
                artist: Some(artist),
            },
        )?;
        ensure!(
            planned.album_pid() == album && planned.artist_pid() == artist,
            "ordinary source groups differ"
        );
        let value: plist::Value = plist::from_bytes(&p.operation)?;
        let root = value
            .as_dictionary()
            .context("ordinary operation envelope missing")?;
        let timestamp: SystemTime = root
            .get("timestamp")
            .and_then(plist::Value::as_date)
            .context("ordinary operation time missing")?
            .into();
        ensure!(
            insert::prepare_live(source, &planned, before, timestamp)? == p.operation,
            "ordinary operation does not reconstruct from source/before"
        );
        let ticks = i64::try_from(source.timing.pcm_samples_per_channel())?;
        Ok(Self {
            title: scalar.title.into(),
            album: scalar.album.into(),
            artist: scalar.artist.into(),
            album_artist: scalar.effective_artist().into(),
            genre: scalar.genre.map(str::to_owned),
            copyright: scalar.copyright.map(str::to_owned),
            year: i64::from(scalar.year.unwrap_or(0)),
            track: i64::from(scalar.track.unwrap_or(0)),
            tracks: i64::from(scalar.track_total.unwrap_or(0)),
            disc: i64::from(scalar.disc.unwrap_or(1)),
            discs: i64::from(scalar.disc_total.unwrap_or(0)),
            millis: ticks
                .checked_mul(1000)
                .context("ordinary timing overflow")?
                / 44100,
            ticks,
            timestamp: timestamp.duration_since(UNIX_EPOCH)?.as_secs_f64(),
            database: before.database_pid() as i64,
            primary: before.primary_container_pid() as i64,
            album_wire: album as i64,
            artist_wire: artist as i64,
        })
    }
}
fn corroborate_dates(
    dates: [f64; 2],
    bytes: u64,
    attributes: &music_protocol::protocol::transfer::Attributes,
) -> Result<()> {
    ensure!(
        attributes.get("st_ifmt").map(String::as_str) == Some("S_IFREG")
            && attributes.get("st_nlink").map(String::as_str) == Some("1")
            && attributes.get("st_size") == Some(&bytes.to_string()),
        "ordinary target physical attributes differ"
    );
    let birth = attributes
        .get("st_birthtime")
        .context("ordinary target birthtime missing")?
        .parse::<u64>()?;
    ensure!(
        birth > 0
            && dates
                .iter()
                .all(|d| d.is_finite() && ((*d + 978307200.0) - birth as f64 / 1e9).abs() <= 120.0),
        "ordinary receiver dates do not match target birthtime"
    );
    Ok(())
}
struct AllocationFacts {
    slot: i64,
    tombstone: Option<Row>,
    maximum: i64,
    generation: i64,
}
/// Source-independent baseline refusals for eligibility. No future row or
/// capability is constructed here.
fn allocation_facts(
    a: &Snapshot,
    database: i64,
    primary: i64,
    anchor: u32,
    budget: &Budget,
) -> Result<AllocationFacts> {
    ensure!(
        a.layout_qualified()?,
        "unqualified ordinary ALAC SQL schema"
    );
    let db = a.table("db_info")?;
    let d = unique(db, "db_pid", Cell::Integer(database))?;
    ensure!(
        db.rows.len() == 1 && int(db, d, "primary_container_pid")? == primary,
        "ordinary ALAC database identity differs"
    );
    unique(a.table("container")?, "sync_id", Cell::Integer(primary))?;
    let props = a.table("_MLDatabaseProperties")?;
    ensure!(
        decimal(props, "MLSyncClientSyncAnchor")? == i64::from(anchor),
        "ordinary ALAC before anchor differs"
    );
    let live = a.table("item")?;
    let slot = live
        .rows
        .keys()
        .map(|r| int(live, r, "item_pid"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .max()
        .context("empty ordinary ALAC library allocation unqualified")?
        .checked_add(1)
        .context("item slot overflow")?;
    let revisions = a.table("entity_revision")?;
    let old: Vec<_> = revisions
        .rows
        .keys()
        .filter(|r| revisions.at(r, "entity_pid").ok() == Some(&Cell::Integer(slot)))
        .collect();
    ensure!(
        old.len() <= 1,
        "new ordinary ALAC slot has multiple old revision identities"
    );
    let tombstone = old.first().map(|r| (*r).clone());
    if let Some(r) = &tombstone {
        ensure!(
            int(revisions, r, "class")? == 0
                && int(revisions, r, "revision_type")? == 0
                && int(revisions, r, "deleted")? == 2,
            "ordinary ALAC allocation slot has unsupported history"
        );
    }
    let mut maximum = 0;
    for (r, n) in &revisions.rows {
        let rev = int(revisions, r, "revision")?;
        ensure!(*n == 1 && rev > 0, "invalid prior revision");
        maximum = maximum.max(rev);
    }
    ensure!(
        maximum > 0 && maximum.checked_add(16).is_some(),
        "revision sequence overflow"
    );
    for k in [
        "MLSyncClientLastSyncedRevision",
        "MLArtistHeroImageImportDatabaseRevision",
    ] {
        let n = decimal(props, k)?;
        ensure!(n > 0 && n <= maximum, "before revision property invalid");
    }
    for (name, t) in &a.tables {
        budget.check()?;
        for (j, key) in t.columns.iter().enumerate() {
            if key.ends_with("_pid") || key == "genre_id" {
                for r in t.rows.keys() {
                    if name == "entity_revision" && tombstone.as_ref() == Some(r) {
                        continue;
                    }
                    ensure!(
                        r[j] != Cell::Integer(slot),
                        "new internal slot still referenced in {name}.{key}"
                    );
                }
            }
        }
    }
    Ok(AllocationFacts {
        slot,
        tombstone,
        maximum,
        generation: decimal(props, "MLSyncClientGenerationID")?,
    })
}
fn before_facts(
    p: &PreparedAttempt,
    w: &Wire,
    a: &Snapshot,
    budget: &Budget,
) -> Result<BeforeFacts> {
    ensure!(
        a.layout_qualified()?,
        "unqualified ordinary ALAC SQL schema"
    );
    let AllocationFacts {
        slot,
        tombstone,
        maximum,
        generation,
    } = allocation_facts(a, w.database, w.primary, p.current_media_anchor, budget)?;
    let live = a.table("item")?;
    let new_wire = [Some(p.content.track_pid), p.new_album_pid, p.new_artist_pid]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    for (name, t) in &a.tables {
        budget.check()?;
        for r in t.rows.keys() {
            for v in r {
                match v {
                    Cell::Integer(i) => ensure!(
                        !new_wire.contains(&(*i as u64)),
                        "new wire ID occurs in {name}"
                    ),
                    Cell::Blob(b) if b.len() == 8 => {
                        let bytes: [u8; 8] = b.as_slice().try_into()?;
                        ensure!(
                            !new_wire.contains(&u64::from_le_bytes(bytes))
                                && !new_wire.contains(&u64::from_be_bytes(bytes)),
                            "new wire ID occurs in binary {name}"
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    let (folder, file) = p
        .content
        .path
        .rsplit_once('/')
        .context("ordinary ALAC path absent")?;
    let loc = a.table("base_location")?;
    let base = int(
        loc,
        unique(loc, "path", Cell::Text(folder.into()))?,
        "base_location_id",
    )?;
    let extra = a.table("item_extra")?;
    for r in extra.rows.keys() {
        if let Cell::Text(old) = extra.at(r, "location")?
            && old.eq_ignore_ascii_case(file)
        {
            ensure!(
                int(
                    live,
                    live.one("item_pid", int(extra, r, "item_pid")?)?,
                    "base_location_id"
                )? != base,
                "ordinary ALAC destination occupied"
            );
        }
    }
    let mut groups = Vec::new();
    for (table, key, label, wire, class, new) in [
        (
            "album",
            "album_pid",
            w.album.as_str(),
            w.album_wire,
            4,
            p.new_album_pid.is_some(),
        ),
        (
            "album_artist",
            "album_artist_pid",
            w.album_artist.as_str(),
            w.artist_wire,
            7,
            p.new_artist_pid.is_some(),
        ),
        (
            "item_artist",
            "item_artist_pid",
            w.artist.as_str(),
            0,
            2,
            false,
        ),
    ] {
        let t = a.table(table)?;
        let mut found = Vec::new();
        for r in t.rows.keys() {
            let matches = if wire != 0 {
                int(t, r, "sync_id")? == wire
            } else {
                same_label(t.at(r, table)?.text()?, label)
            };
            if matches {
                found.push(r);
            }
        }
        ensure!(found.len() <= 1, "ambiguous ordinary ALAC {table}");
        if wire != 0 {
            ensure!(
                new == found.is_empty(),
                "selected {table} new/live state differs"
            );
        }
        ensure!(
            found.is_empty(),
            "ordinary reused-group SQL profile not yet qualified"
        );
        groups.push(Group {
            table,
            key,
            label: label.to_owned(),
            sort_label: label.to_owned(),
            wire,
            class,
        });
    }
    if let Some(label) = &w.genre {
        ensure!(
            a.table("genre")?.rows.keys().all(|r| a
                .table("genre")
                .and_then(|t| t.at(r, "genre"))
                .and_then(Cell::text)
                .is_ok_and(|old| !same_label(old, label))),
            "ordinary reused genre SQL branch not yet qualified"
        );
        groups.push(Group {
            table: "genre",
            key: "genre_id",
            label: label.clone(),
            sort_label: label.clone(),
            wire: 0,
            class: 3,
        });
    }
    Ok(BeforeFacts {
        slot,
        tombstone,
        maximum,
        generation,
        base,
        groups,
    })
}
fn same_label(a: &str, b: &str) -> bool {
    a == b || (a.is_ascii() && b.is_ascii() && a.eq_ignore_ascii_case(b))
}
