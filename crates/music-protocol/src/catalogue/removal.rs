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
pub use playlist_deletion::{
    PlaylistDeletionReceipt, deletion_not_applied, qualify_clear, qualify_deletion,
    verify_deletion, verify_deletion_later,
};

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

/// Before-only qualification, run before sending and again at verification:
/// every target is one distinct live item whose relationships are all
/// modelled here. Container memberships are allowed in the library container
/// and Playback History, which the receiver leaves in place, and once per
/// ordinary playlist of the modelled shape, which loses them.
pub fn qualify(before: &Tables, targets: &[i64]) -> Result<()> {
    ensure!(!targets.is_empty(), "removal has no targets");
    let distinct: BTreeSet<_> = targets.iter().collect();
    ensure!(
        distinct.len() == targets.len() && targets.iter().all(|t| *t > 0),
        "removal targets must be distinct nonzero items"
    );
    let retained = retained_containers(before)?;
    let playlists = playlists::ordinary(before)?;
    let mut memberships = BTreeSet::new();
    for (name, table) in before {
        let Ok(key) = table.column("item_pid") else {
            continue;
        };
        if ITEM_TABLES.contains(&name.as_str()) {
            continue;
        }
        let container = table.column("container_pid").ok();
        for row in table.rows.keys() {
            let Cell::Integer(id) = row[key] else {
                continue;
            };
            if !distinct.contains(&id) {
                continue;
            }
            let owner = match container.map(|c| &row[c]) {
                Some(Cell::Integer(pid)) if name == "container_item" => Some(*pid),
                _ => None,
            };
            let allowed = owner.is_some_and(|pid| {
                retained.contains(&pid) || playlists.contains(&pid) && memberships.insert((pid, id))
            });
            ensure!(
                allowed,
                "removal target {id} has an unmodelled {name} relationship"
            );
        }
    }
    let items = table(before, "item")?;
    for item in targets {
        let row = items.one("item_pid", *item)?;
        for (name, key, _) in GROUPS {
            let id = typed::int(items, row, key)?;
            if id != 0 {
                table(before, name)?.one(key, id)?;
            }
        }
        let (revision, number) = target_revision(before, *item)?;
        let revisions = table(before, "entity_revision")?;
        ensure!(
            number > 0 && typed::int(revisions, revision, "deleted")? == 0,
            "removal target {item} revision is not live"
        );
        target_stats_revision(before, *item)?;
    }
    playlists::qualify(before, &targets.iter().copied().collect())?;
    for key in [ANCHOR, LAST_SYNCED, GENERATION, HERO] {
        property(before, key)?;
    }
    Ok(())
}

/// `qualify` for the removals of a session that also inserts tracks. That
/// session is verified against the catalogue, which holds no playlist, so no
/// target may belong to an ordinary playlist; such removals go in a session
/// of their own.
pub fn qualify_session(before: &Tables, targets: &[i64]) -> Result<()> {
    qualify(before, targets)?;
    let holding = playlists::holding(before, &targets.iter().copied().collect())?;
    ensure!(
        holding.is_empty(),
        "{} ordinary playlist(s) hold a removal target; a session that also inserts removes no playlist member",
        holding.len()
    );
    Ok(())
}

pub fn verify(
    before: &Tables,
    after: &Tables,
    targets: &[i64],
    base_anchor: u32,
) -> Result<RemovalReceipt> {
    verify_with(before, after, targets, base_anchor, None)
}

/// `verify` for a later run's capture, after the phone may have been used:
/// everything the session wrote is modelled as before, except that the
/// survivors' renumbering is left to `between_runs`, since the phone may
/// renumber them again later. The session's tombstones, moved album and
/// playlist revisions and keep-local evaluations stay exact.
pub fn verify_later(
    before: &Tables,
    after: &Tables,
    targets: &[i64],
    base_anchor: u32,
    between_runs: BetweenRuns<'_>,
) -> Result<RemovalReceipt> {
    verify_with(before, after, targets, base_anchor, Some(between_runs))
}

fn verify_with(
    before: &Tables,
    after: &Tables,
    targets: &[i64],
    base_anchor: u32,
    between_runs: Option<BetweenRuns<'_>>,
) -> Result<RemovalReceipt> {
    qualify(before, targets)?;
    ensure!(
        before.keys().eq(after.keys())
            && before.iter().all(|(n, t)| {
                t.columns == after[n].columns && t.definitions == after[n].definitions
            }),
        "removal table set or column definitions differ"
    );
    ensure!(
        decimal(property(before, ANCHOR)?, false)? == i64::from(base_anchor),
        "removal before state is not at the prepared anchor"
    );
    let removed: BTreeSet<i64> = targets.iter().copied().collect();
    let retained = retained_containers(before)?;
    let mut expected = before.clone();
    let mut playlist_removals =
        playlists::remove_memberships(before, after, &mut expected, &removed)?;
    let mut evaluations =
        playlists::evaluate_untouched(before, after, &mut expected, &playlist_removals)?;
    let mut receipts = Vec::new();
    for item in targets {
        let target = Cell::Integer(*item);
        let mut removed_rows = BTreeMap::new();
        for name in ITEM_TABLES {
            let t = expected.get_mut(name).context("item table absent")?;
            let key = t.column("item_pid")?;
            let mut count = 0;
            t.rows.retain(|r, n| {
                let own = r[key] == target;
                if own {
                    count += *n;
                }
                !own
            });
            if count > 0 {
                removed_rows.insert(name, count);
            }
        }
        let mut removed_artwork_rows = 0;
        for name in ["artwork_token", "best_artwork_token"] {
            let t = expected.get_mut(name).context("artwork table absent")?;
            let id = t.column("entity_pid")?;
            let kind = t.column("entity_type")?;
            t.rows.retain(|r, n| {
                let own = r[id] == target && r[kind] == Cell::Integer(0);
                if own {
                    removed_artwork_rows += *n;
                }
                !own
            });
        }
        let mut retained_memberships = 0;
        if let Some(memberships) = before.get("container_item") {
            let key = memberships.column("item_pid")?;
            let owner = memberships.column("container_pid")?;
            retained_memberships = memberships
                .rows
                .iter()
                .filter(|(r, _)| {
                    r[key] == target
                        && matches!(r[owner], Cell::Integer(pid) if retained.contains(&pid))
                })
                .map(|(_, n)| *n)
                .sum();
        }
        receipts.push(TargetRemoval {
            item_pid: *item,
            removed_rows,
            tombstone_revision_before: 0,
            tombstone_revision_after: 0,
            removed_stats_revision: None,
            removed_artwork_rows,
            retained_memberships,
        });
    }
    // Token rows whose entity was already gone before the session may go.
    let mut removed_orphan_artwork_rows = 0;
    for name in ["artwork_token", "best_artwork_token"] {
        for (row, n) in orphan_tokens(before, name)? {
            if !table(after, name)?.rows.contains_key(&row) {
                expected
                    .get_mut(name)
                    .context("artwork table absent")?
                    .rows
                    .remove(&row);
                removed_orphan_artwork_rows += n;
            }
        }
    }
    let survivors = item_ids(table(&expected, "item")?)?;
    let mut collected = collected_groups(before, after, &survivors)?;
    for group in &collected {
        let t = expected
            .get_mut(group.table)
            .context("group table absent")?;
        let key = t.column(group.key)?;
        t.rows
            .retain(|r, _| r[key] != Cell::Integer(group.group_pid));
    }

    let recent_counter_resets = recent_counter_resets(before, after, &mut expected, &removed)?;
    let filled_phone_entries = fill_phone_entries(before, after, &mut expected)?;

    // Each target's live revision becomes its tombstone above the last synced
    // one; every tombstone number is distinct.
    let last_synced = decimal(property(before, LAST_SYNCED)?, false)?;
    let revisions = table(before, "entity_revision")?;
    let mut numbers = BTreeSet::new();
    for receipt in &mut receipts {
        let item = receipt.item_pid;
        let (old_revision, old_number) = target_revision(before, item)?;
        let (new_revision, new_number) = target_revision(after, item)?;
        ensure!(
            new_number > old_number
                && new_number > last_synced
                && numbers.insert(new_number)
                && typed::int(revisions, new_revision, "deleted")? == 2,
            "removal target {item} tombstone is invalid"
        );
        let mut tombstone = old_revision.clone();
        typed::set(
            revisions,
            &mut tombstone,
            "revision",
            Cell::Integer(new_number),
        )?;
        typed::set(revisions, &mut tombstone, "deleted", Cell::Integer(2))?;
        ensure!(
            tombstone == *new_revision,
            "removal target {item} tombstone changed other fields"
        );
        typed::replace(&mut expected, "entity_revision", old_revision, tombstone)?;
        receipt.tombstone_revision_before = old_number;
        receipt.tombstone_revision_after = new_number;
        if let Some((stats_row, number)) = target_stats_revision(before, item)? {
            if !table(after, "entity_revision")?
                .rows
                .contains_key(stats_row)
            {
                ensure!(
                    new_number > number
                        && expected
                            .get_mut("entity_revision")
                            .context("revisions absent")?
                            .rows
                            .remove(stats_row)
                            == Some(1),
                    "removal target {item} stats revision is not dominated"
                );
                receipt.removed_stats_revision = Some(number);
            }
        }
    }
    // A collected group's revisions follow the same rule as a target's.
    for group in &mut collected {
        let class = group.class;
        let own = |t: &Table| -> Result<Vec<(Row, i64)>> {
            let mut rows = Vec::new();
            for (row, n) in &t.rows {
                if typed::int(t, row, "entity_pid")? == group.group_pid
                    && typed::int(t, row, "class")? == class
                {
                    ensure!(*n == 1, "duplicate group revision");
                    rows.push((row.clone(), typed::int(t, row, "revision_type")?));
                }
            }
            Ok(rows)
        };
        let after_revisions = table(after, "entity_revision")?;
        let now = own(after_revisions)?;
        for (old, kind) in own(revisions)? {
            let number = typed::int(revisions, &old, "revision")?;
            match kind {
                0 => {
                    let (tombstone, _) = now.iter().find(|(_, k)| *k == 0).with_context(|| {
                        format!(
                            "collected {} {} has no tombstone",
                            group.table, group.group_pid
                        )
                    })?;
                    let after_number = typed::int(after_revisions, tombstone, "revision")?;
                    let mut desired = old.clone();
                    typed::set(
                        revisions,
                        &mut desired,
                        "revision",
                        Cell::Integer(after_number),
                    )?;
                    typed::set(revisions, &mut desired, "deleted", Cell::Integer(2))?;
                    ensure!(
                        typed::int(revisions, &old, "deleted")? == 0
                            && after_number > number
                            && after_number > last_synced
                            && numbers.insert(after_number)
                            && desired == *tombstone,
                        "collected {} {} tombstone is invalid",
                        group.table,
                        group.group_pid
                    );
                    typed::replace(&mut expected, "entity_revision", &old, desired)?;
                    group.tombstone = Some((number, after_number));
                }
                1 => {
                    ensure!(
                        now.iter().all(|(_, k)| *k != 1)
                            && expected
                                .get_mut("entity_revision")
                                .context("revisions absent")?
                                .rows
                                .remove(&old)
                                == Some(1),
                        "collected {} {} stats revision stayed",
                        group.table,
                        group.group_pid
                    );
                    group.removed_stats_revision = Some(number);
                }
                other => bail!("unmodelled group revision type {other}"),
            }
        }
        ensure!(
            group.tombstone.is_some(),
            "collected {} {} has no live revision",
            group.table,
            group.group_pid
        );
    }
    playlists::move_revisions(
        before,
        after,
        &mut expected,
        &mut playlist_removals,
        last_synced,
        &mut numbers,
    )?;
    let highest = *numbers.last().context("removal has no tombstones")?;

    // Sync properties: exactly one anchor, the highest tombstone or playlist
    // revision as last synced, and a free generation. Hero progress may follow or stay; it is
    // checked below, once every revision the session wrote is known.
    let anchor = base_anchor
        .checked_add(1)
        .context("removal anchor exhausted")?;
    ensure!(
        decimal(property(after, ANCHOR)?, false)? == i64::from(anchor)
            && decimal(property(after, LAST_SYNCED)?, false)? == highest,
        "removal anchor/last-synced properties differ"
    );
    let hero = decimal(property(after, HERO)?, false)?;
    let property_changes = property_changes(before, after, &mut expected)?;

    // Groups stay. A representative that was a target moves to a surviving
    // member, or to zero when none survives; album representatives may move to
    // any surviving same-album member. An artist or genre whose
    // representative has no cover of its own may move to its lowest member
    // that has one: one removal moved an artist's representative off the
    // library's one track without a cover, although no target belonged to
    // that artist, and another moved a genre's off a coverless test tone
    // with its album and artists. No other group cell may change.
    let items = table(before, "item")?;
    let tokens = table(before, "artwork_token")?;
    let mut covered = BTreeSet::new();
    for row in tokens.rows.keys() {
        if typed::int(tokens, row, "entity_type")? == 0 {
            covered.insert(typed::int(tokens, row, "entity_pid")?);
        }
    }
    let mut representatives = Vec::new();
    let mut emptied = BTreeSet::new();
    for (name, key, _) in GROUPS {
        let mut members: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
        let mut touched = BTreeSet::new();
        for row in items.rows.keys() {
            let id = typed::int(items, row, "item_pid")?;
            let group = typed::int(items, row, key)?;
            if removed.contains(&id) {
                touched.insert(group);
            } else {
                members.entry(group).or_default().insert(id);
            }
        }
        if name == "album" {
            emptied.extend(touched.iter().filter(|g| !members.contains_key(g)));
        }
        let groups = table(before, name)?;
        let after_groups = table(after, name)?;
        for (row, n) in &groups.rows {
            ensure!(*n == 1, "duplicate {name} row");
            let id = typed::int(groups, row, key)?;
            if collected
                .iter()
                .any(|g| g.table == name && g.group_pid == id)
            {
                continue;
            }
            let now_row = after_groups
                .one(key, id)
                .with_context(|| format!("removal deleted {name} {id}"))?;
            let prior = typed::int(groups, row, "representative_item_pid")?;
            let now = typed::int(after_groups, now_row, "representative_item_pid")?;
            if prior == now {
                continue;
            }
            let surviving = members.get(&id);
            let allowed = if removed.contains(&prior) {
                surviving.map_or(now == 0, |s| s.contains(&now))
            } else if name == "album" {
                surviving.is_some_and(|s| s.contains(&now))
            } else {
                matches!(name, "album_artist" | "item_artist" | "genre")
                    && !covered.contains(&prior)
                    && surviving.and_then(|s| s.iter().find(|m| covered.contains(*m))) == Some(&now)
            };
            ensure!(
                allowed,
                "unqualified {name} {id} representative {prior} -> {now}"
            );
            let mut desired = row.clone();
            typed::set(
                groups,
                &mut desired,
                "representative_item_pid",
                Cell::Integer(now),
            )?;
            typed::replace(&mut expected, name, row, desired)?;
            representatives.push(Representative {
                table: name,
                group_pid: id,
                before: prior,
                after: now,
            });
        }
    }

    let before_albums = table(before, "album")?;
    let after_albums = table(after, "album")?;
    let evaluated = unevaluated_albums(before_albums, after_albums, &emptied, &collected)?;
    let recomputed: BTreeSet<i64> = emptied.union(&evaluated).copied().collect();
    let mut emptied_albums = BTreeMap::new();
    for id in &recomputed {
        let Ok(row) = table(&expected, "album")?.one("album_pid", *id) else {
            continue;
        };
        let row = row.clone();
        let now = after_albums.one("album_pid", *id)?.clone();
        if row == now {
            continue;
        }
        let column = before_albums.column("keep_local_status")?;
        let mut desired = row.clone();
        desired[column] = now[column].clone();
        ensure!(
            desired == now && now[column] == Cell::Integer(2),
            "album {id} changed beyond keep_local_status 2"
        );
        let prior = typed::int(before_albums, &row, "keep_local_status")?;
        typed::replace(&mut expected, "album", &row, desired)?;
        emptied_albums.insert(
            *id,
            EmptiedAlbum {
                album_pid: *id,
                keep_local_status: Some((prior, 2)),
                revision: None,
                removed_stats_revision: None,
            },
        );
    }

    // Keep-local evaluations land above everything else the session wrote;
    // the survivor refresh is compared without them.
    let keep_local = playlists::keep_local_rows(after, &playlist_removals, &evaluations)?;
    let mut actual_revisions = table(after, "entity_revision")?.clone();
    for (_, row, _) in &keep_local {
        actual_revisions.rows.remove(row);
    }
    let (survivor_revision_refresh, album_revisions) = survivor_revision_refresh(
        expected
            .get_mut("entity_revision")
            .context("revisions absent")?,
        &actual_revisions,
        &survivors,
        &recomputed,
        highest,
        between_runs.is_some(),
    )?;
    // Hero progress stays, follows the highest tombstone, or follows the
    // highest revision written at all: an emptied album's new revision lies
    // above the tombstones (observed).
    let written = survivor_revision_refresh
        .iter()
        .map(|r| r.after)
        .chain(album_revisions.values().map(|(_, after, _)| *after))
        .fold(highest, i64::max);
    // A later run cannot tell the session's survivor refresh from the
    // phone's own renumbering since, so hero progress may stop anywhere
    // above the tombstones the after state reaches.
    let reached = |hero: i64| -> Result<bool> {
        let revisions = table(after, "entity_revision")?;
        for row in revisions.rows.keys() {
            if typed::int(revisions, row, "revision")? >= hero {
                return Ok(true);
            }
        }
        Ok(false)
    };
    ensure!(
        hero == highest
            || hero == written
            || hero == decimal(property(before, HERO)?, false)?
            || between_runs.is_some() && hero > highest && reached(hero)?,
        "removal hero revision progress differs"
    );
    playlists::add_keep_local(
        &mut expected,
        &mut playlist_removals,
        &mut evaluations,
        keep_local,
        written,
    )?;
    for (id, (before, after, stats)) in album_revisions {
        let album = emptied_albums.entry(id).or_insert(EmptiedAlbum {
            album_pid: id,
            keep_local_status: None,
            revision: None,
            removed_stats_revision: None,
        });
        album.revision = Some((before, after));
        album.removed_stats_revision = stats;
    }
    for id in &evaluated {
        ensure!(
            emptied_albums.get(id).is_some_and(|a| a.revision.is_some()),
            "evaluated album {id} kept its revision"
        );
    }
    let (evaluated_albums, emptied_albums): (Vec<_>, Vec<_>) = emptied_albums
        .into_values()
        .partition(|album| evaluated.contains(&album.album_pid));

    let between_runs_playback = match between_runs {
        Some(absorb) => absorb(&mut expected, after, highest)?,
        None => None,
    };
    let mismatches: Vec<_> = expected
        .iter()
        .filter(|(n, t)| t.rows != after[*n].rows)
        .map(|(n, _)| n.as_str())
        .collect();
    ensure!(
        mismatches.is_empty(),
        "unmodelled removal rows in tables: {}",
        mismatches.join(", ")
    );
    Ok(RemovalReceipt {
        schema: "music-protocol.removal-sql",
        targets: receipts,
        property_changes,
        representatives,
        recent_counter_resets,
        filled_phone_entries,
        removed_orphan_artwork_rows,
        survivor_revision_refresh,
        emptied_albums,
        evaluated_albums,
        collected_groups: collected,
        playlists: playlist_removals,
        keep_local_evaluations: evaluations,
        between_runs_playback,
    })
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

mod playlist_deletion;
pub mod playlists;
#[cfg(test)]
mod single_target_tests;
#[cfg(test)]
mod tests;
