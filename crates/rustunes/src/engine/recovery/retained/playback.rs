//! Recovery-only SQL evidence after a separately verified original transition.
//! Every recovery also models one advancing maintenance clock property.
//! This proves no new Media revision or unrelated SQL change. It grants no
//! transport, replay, journal transition, or permission to normalize snapshots.
use crate::{
    engine::execution::native_insert::Control,
    engine::state::current::Current,
    engine::verification::sql_tables::{
        Capture, SqlHashes,
        reader::{self, Budget, Cell, Row, Snapshot, Table},
    },
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

mod ordinary_stats;

mod ordinary_maintenance;

pub(crate) mod between_runs;
pub(crate) use between_runs::Window;

pub(super) struct VerifiedPlaybackBridge {
    receipt: Value,
    sha256: [u8; 32],
}
impl VerifiedPlaybackBridge {
    pub(super) fn evidence(&self) -> Value {
        json!({"receipt_sha256":reader::hex(&self.sha256),"receipt":self.receipt})
    }
}

pub(super) fn verify(
    immediate: &Current,
    immediate_dir: &Path,
    fresh: &Current,
    fresh_dir: &Path,
    output: &Path,
    deadline: Instant,
) -> Result<VerifiedPlaybackBridge> {
    same_scope(immediate, fresh)?;
    reader::directory(output)?;
    let mut budget = Budget::new_until(deadline);
    let old = materialize(
        immediate,
        immediate_dir,
        &output.join("immediate"),
        &mut budget,
    )?;
    let new = materialize(fresh, fresh_dir, &output.join("fresh"), &mut budget)?;
    let delta = reader::delta(&old, &new, &budget)?;
    let delta_sha = reader::durable(
        &output.join("full-sql-delta.json"),
        &serde_json::to_vec_pretty(&delta)?,
    )?;
    let changes = match verify_views(&old, &new, &budget) {
        Ok(changes) => changes,
        Err(error) => {
            reader::durable(
                &output.join("refused.json"),
                &serde_json::to_vec_pretty(&json!({
                    "schema":"rustunes.playback-bridge-refusal",
                    "reason":format!("{error:#}"),"full_delta_sha256":reader::hex(&delta_sha),
                    "phone_writes":0,"journal_completion_authorized":false,
                }))?,
            )?;
            return Err(error);
        }
    };
    reader::recheck(&old, &budget)?;
    reader::recheck(&new, &budget)?;
    let receipt = json!({
        "schema":"rustunes.artwork-recovery-playback-bridge",
        "device_sha256":reader::hex(&fresh.identity.device_digest()),
        "library_sha256":reader::hex(&fresh.identity.library_digest()),
        "immediate_state_receipt_sha256":reader::hex(&immediate.receipt_sha256),
        "fresh_state_receipt_sha256":reader::hex(&fresh.receipt_sha256),
        "immediate_sql":old.hashes,"fresh_sql":new.hashes,
        "media_anchor":fresh.catalog.persisted_media_anchor(),
        "database_pid":fresh.catalog.database_pid(),"primary_container_pid":fresh.catalog.primary_container_pid(),
        "schema_sha256":old.schema_hash()?,"table_count":45,"changes":changes,
        "full_delta_sha256":reader::hex(&delta_sha),"nonplayback_sql_exact":true,
        "prefs_cdb_raw_artwork_exact":true,"new_media_revision":false,
        "fresh_playback_equal_to_pre_import_claimed":false,
        "history_and_completed_play_counters_validated_independently":true,
        "live_revision_bookkeeping_validated_separately":true,
        "phone_writes":0,"replay_authorized":false,"journal_completion_authorized":false,
    });
    let sha256 = reader::durable(
        &output.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(VerifiedPlaybackBridge { receipt, sha256 })
}

fn same_scope(a: &Current, b: &Current) -> Result<()> {
    ensure!(
        a.device == b.device
            && a.identity.device_digest() == b.identity.device_digest()
            && a.identity.library_digest() == b.identity.library_digest()
            && a.raw_preferences == b.raw_preferences
            && a.plist_preferences == b.plist_preferences
            && a.cdb == b.cdb
            && a.artwork_db.is_some()
            && a.artwork_db == b.artwork_db
            && a.catalog.database_pid() == b.catalog.database_pid()
            && a.catalog.primary_container_pid() == b.catalog.primary_container_pid()
            && a.catalog.persisted_media_anchor() == b.catalog.persisted_media_anchor()
            && a.catalog.tracks().len() == b.catalog.tracks().len(),
        "playback bridge state identity, Media revision, prefs, CDB, or artwork differs"
    );
    a.library.ensure_matches(&b.library)
}

/// Historical ordinary captures may omit raw ArtworkDB. A fresh capture may
/// add it, but cannot discard or change a captured baseline. Missing historical
/// evidence is disclosed and requires managed physical preservation.
pub(super) fn same_ordinary_scope(a: &Current, b: &Current) -> Result<()> {
    ensure!(
        a.device == b.device
            && a.identity.device_digest() == b.identity.device_digest()
            && a.identity.library_digest() == b.identity.library_digest()
            && a.raw_preferences == b.raw_preferences
            && a.plist_preferences == b.plist_preferences
            && a.cdb == b.cdb
            && a.catalog.database_pid() == b.catalog.database_pid()
            && a.catalog.primary_container_pid() == b.catalog.primary_container_pid()
            && a.catalog.persisted_media_anchor() == b.catalog.persisted_media_anchor()
            && a.catalog.tracks().len() == b.catalog.tracks().len()
            && a.catalog
                .tracks()
                .iter()
                .map(|t| t.sync_id)
                .collect::<BTreeSet<_>>()
                == b.catalog
                    .tracks()
                    .iter()
                    .map(|t| t.sync_id)
                    .collect::<BTreeSet<_>>(),
        "ordinary playback Current identity, membership, revision, prefs, CDB or captured artwork differs"
    );
    ordinary_raw_artwork_comparison(a, b)?;
    a.library.ensure_matches(&b.library)
}

pub(super) fn ordinary_raw_artwork_comparison(
    immediate: &Current,
    fresh: &Current,
) -> Result<&'static str> {
    match (&immediate.artwork_db, &fresh.artwork_db) {
        (Some(before), Some(after)) if before == after => Ok("captured_equal"),
        (None, Some(_)) => Ok("not_captured_in_immediate_state"),
        (None, None) => Ok("absent_from_both_captures"),
        _ => anyhow::bail!("ordinary playback captured artwork changed or is missing"),
    }
}

pub(super) fn verify_ordinary(
    immediate: &Current,
    immediate_dir: &Path,
    fresh: &Current,
    fresh_dir: &Path,
    output: &Path,
    control: &Control,
) -> Result<VerifiedPlaybackBridge> {
    control.check()?;
    same_ordinary_scope(immediate, fresh)?;
    reader::directory(output)?;
    let mut budget =
        Budget::new_cancellable_until(control.deadline.into_std(), Arc::clone(&control.cancelled));
    let old = materialize_profile(
        immediate,
        immediate_dir,
        &output.join("immediate"),
        &mut budget,
        true,
    )?;
    let new = materialize_profile(fresh, fresh_dir, &output.join("fresh"), &mut budget, true)?;
    let delta = serde_json::to_vec_pretty(&reader::delta(&old, &new, &budget)?)?;
    ensure!(
        delta.len() <= 256 << 20,
        "ordinary playback delta exceeds bound"
    );
    let delta_sha = reader::durable(&output.join("full-sql-delta.json"), &delta)?;
    let changes = verify_views(&old, &new, &budget);
    reader::recheck(&old, &budget)?;
    reader::recheck(&new, &budget)?;
    let changes = match changes {
        Ok(changes) => changes,
        Err(error) => {
            reader::durable(
                &output.join("refused.json"),
                &serde_json::to_vec_pretty(&json!({
                    "schema":"rustunes.ordinary-playback-refusal",
                    "reason":format!("{error:#}"),"full_delta_sha256":reader::hex(&delta_sha),
                    "phone_writes":0,"journal_completion_authorized":false,
                }))?,
            )?;
            return Err(error);
        }
    };
    control.check()?;
    let nonplayback_exact = changes["maintenance_task_operation_date"].is_null();
    let receipt = json!({
        "schema":"rustunes.ordinary-recovery-playback-bridge",
        "device_sha256":reader::hex(&fresh.identity.device_digest()),
        "library_sha256":reader::hex(&fresh.identity.library_digest()),
        "immediate_state_receipt_sha256":reader::hex(&immediate.receipt_sha256),
        "fresh_state_receipt_sha256":reader::hex(&fresh.receipt_sha256),
        "immediate_sql":old.hashes,"fresh_sql":new.hashes,
        "media_anchor":fresh.catalog.persisted_media_anchor(),
        "database_pid":fresh.catalog.database_pid(),"primary_container_pid":fresh.catalog.primary_container_pid(),
        "schema_sha256":old.schema_hash()?,"table_count":45,"changes":changes,
        "full_delta_sha256":reader::hex(&delta_sha),"nonplayback_sql_exact":nonplayback_exact,
        "nonplayback_sql_exact_except":if nonplayback_exact { Vec::<&str>::new() } else {
            vec!["_MLDatabaseProperties.MLMaintenanceTaskOperationDate"]
        },
        "prefs_and_cdb_exact":true,
        "raw_artwork_comparison":ordinary_raw_artwork_comparison(immediate, fresh)?,
        "all_managed_physical_preservation_still_required":true,
        "new_media_revision":false,"history_and_completed_counters_validated_independently":true,
        "phone_writes":0,"replay_authorized":false,"journal_completion_authorized":false,
    });
    let sha256 = reader::durable(
        &output.join("receipt.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    control.check()?;
    Ok(VerifiedPlaybackBridge { receipt, sha256 })
}

fn materialize(
    current: &Current,
    directory: &Path,
    output: &Path,
    budget: &mut Budget,
) -> Result<Snapshot> {
    materialize_profile(current, directory, output, budget, false)
}

fn materialize_profile(
    current: &Current,
    directory: &Path,
    output: &Path,
    budget: &mut Budget,
    controlled: bool,
) -> Result<Snapshot> {
    let database = directory.join("catalog/MediaLibrary.sqlitedb");
    let wal = directory.join("catalog/MediaLibrary.sqlitedb-wal");
    let h = current.catalog.source_hashes();
    let capture = Capture {
        database: &database,
        database_sha256: h.database,
        wal: h.wal.map(|hash| (wal.as_path(), hash)),
    };
    let result = if controlled {
        reader::materialize_with_control(capture, output, budget)?
    } else {
        reader::materialize(capture, output, budget)?
    };
    let expected = SqlHashes {
        database_sha256: reader::hex(&h.database),
        database_bytes: h.database_bytes,
        wal_sha256: h.wal.map(|hash| reader::hex(&hash)),
        wal_bytes: h.wal_bytes,
    };
    ensure!(
        result.hashes == expected,
        "playback bridge SQL differs from bound Current"
    );
    Ok(result)
}

/// SQL evidence only. The caller must separately prove the original write,
/// Current scope, physical files and original transcript before any completion.
/// Completed plays and skips, Playback History, revision housekeeping and the
/// maintenance clock are the only accepted differences; no authority follows.
pub(super) fn verify_views(a: &Snapshot, b: &Snapshot, budget: &Budget) -> Result<Value> {
    budget.check()?;
    ensure!(
        a.layout_qualified()? && b.version == a.version && a.schema == b.schema,
        "playback bridge has unsupported or changed full SQL schema"
    );
    verify_tables(&a.tables, &b.tables, budget)
}

fn verify_tables(
    a: &BTreeMap<String, Table>,
    b: &BTreeMap<String, Table>,
    budget: &Budget,
) -> Result<Value> {
    ensure!(a.keys().eq(b.keys()), "playback bridge tables differ");
    let mut maintenance = None;
    for (name, old) in a {
        budget.check()?;
        let new = &b[name];
        ensure!(
            old.columns == new.columns && old.definitions == new.definitions,
            "playback bridge table definition differs: {name}"
        );
        if name == "_MLDatabaseProperties" {
            maintenance = ordinary_maintenance::check(old, new, budget)?;
        } else if ![
            "item_stats",
            "container",
            "container_item",
            "entity_revision",
        ]
        .contains(&name.as_str())
        {
            ensure!(old.rows == new.rows, "nonplayback SQL changed in {name}");
        }
    }
    let table = |tables: &BTreeMap<String, Table>, name: &str| {
        tables
            .get(name)
            .cloned()
            .with_context(|| format!("missing playback table {name}"))
    };
    let items = keyed(&table(a, "item")?, "item_pid")?;
    let old_stats = table(a, "item_stats")?;
    let new_stats = table(b, "item_stats")?;
    let stats = ordinary_stats::check(&old_stats, &new_stats, &items, budget)?;
    let history = check_history(
        &table(a, "container")?,
        &table(b, "container")?,
        &table(a, "container_item")?,
        &table(b, "container_item")?,
        &items,
        budget,
    )?;
    let revisions = check_revisions(
        None,
        &table(a, "entity_revision")?,
        &table(b, "entity_revision")?,
        &stats,
        &items,
        history.changed_container,
        budget,
    )?;
    let events = stats.values().collect::<Vec<_>>();
    Ok(
        json!({"completed_events":events,"playback_history":history.report,
        "entity_revisions":revisions,"maintenance_task_operation_date":maintenance}),
    )
}

fn keyed(t: &Table, key: &str) -> Result<BTreeMap<i64, Row>> {
    let mut result = BTreeMap::new();
    for (row, count) in &t.rows {
        ensure!(
            *count == 1 && result.insert(num(t, row, key)?, row.clone()).is_none(),
            "ambiguous playback {key}"
        );
    }
    Ok(result)
}
fn num(t: &Table, r: &Row, key: &str) -> Result<i64> {
    t.at(r, key)?.integer()
}
fn number(cell: &Cell) -> Result<f64> {
    let n = match cell {
        Cell::Integer(v) => *v as f64,
        Cell::Real(bits) => f64::from_bits(*bits),
        _ => anyhow::bail!("playback date is not numeric"),
    };
    ensure!(
        n.is_finite() && n >= 0.0,
        "playback date is negative or nonfinite"
    );
    Ok(n)
}
fn exact_except(t: &Table, a: &Row, b: &Row, allowed: &[&str]) -> Result<()> {
    ensure!(
        a.len() == t.columns.len() && b.len() == a.len(),
        "malformed playback row"
    );
    for (i, column) in t.columns.iter().enumerate() {
        ensure!(
            a[i] == b[i] || allowed.contains(&column.as_str()),
            "unrecognized playback change: {column}"
        );
    }
    Ok(())
}
#[derive(serde::Serialize)]
struct Played {
    item_pid: i64,
    play_count_delta: u64,
    skip_count_delta: u64,
    before: BTreeMap<String, Cell>,
    after: BTreeMap<String, Cell>,
}
struct History {
    changed_container: Option<i64>,
    report: Value,
}
fn check_history(
    a: &Table,
    b: &Table,
    ai: &Table,
    bi: &Table,
    items: &BTreeMap<i64, Row>,
    budget: &Budget,
) -> Result<History> {
    let old = keyed(a, "container_pid")?;
    let new = keyed(b, "container_pid")?;
    ensure!(
        old.keys().eq(new.keys()),
        "container membership changed after artwork"
    );
    let candidates = old
        .iter()
        .filter(|(_, r)| num(a, r, "distinguished_kind").ok() == Some(10000))
        .collect::<Vec<_>>();
    ensure!(
        candidates.len() <= 1,
        "ambiguous Playback History container"
    );
    let Some((&pid, row)) = candidates.first().copied() else {
        ensure!(
            a.rows == b.rows && ai.rows == bi.rows,
            "unknown playlist changed"
        );
        return Ok(History {
            changed_container: None,
            report: json!(null),
        });
    };
    ensure!(
        a.at(row, "name")?.text()? == "Playback History"
            && num(a, row, "is_hidden")? == 1
            && num(a, row, "is_editable")? == 0
            && num(a, row, "is_owner")? == 1
            && num(a, row, "contained_media_type")? == 8
            && num(a, row, "is_src_remote")? == 0
            && num(a, row, "sync_id")? == 0,
        "unsupported Playback History container shape"
    );
    for (id, before) in &old {
        budget.check()?;
        if *id != pid {
            ensure!(before == &new[id], "unrelated container changed");
        } else {
            exact_except(a, before, &new[id], &["date_modified"])?;
        }
    }
    let all_old = keyed(ai, "container_item_pid")?;
    let all_new = keyed(bi, "container_item_pid")?;
    let partition =
        |t: &Table, rows: &BTreeMap<i64, Row>| -> Result<(BTreeMap<i64, Row>, Vec<Row>)> {
            let mut other = BTreeMap::new();
            let mut history = BTreeMap::new();
            for (id, r) in rows {
                budget.check()?;
                if num(t, r, "container_pid")? == pid {
                    let position = usize::try_from(num(t, r, "position")?)?;
                    ensure!(
                        position < 50_000 && history.insert(position, r.clone()).is_none(),
                        "history positions are ambiguous or out of bounds"
                    );
                } else {
                    other.insert(*id, r.clone());
                }
            }
            ensure!(
                history.keys().copied().eq(0..history.len()),
                "history positions are not contiguous"
            );
            Ok((other, history.into_values().collect()))
        };
    let (other_old, history_old) = partition(ai, &all_old)?;
    let (other_new, history_new) = partition(bi, &all_new)?;
    ensure!(
        other_old == other_new,
        "unrelated playlist membership changed"
    );
    let container_changed = row != &new[&pid];
    let queue_changed = history_old != history_new;
    if !container_changed && !queue_changed {
        return Ok(History {
            changed_container: None,
            report: json!(null),
        });
    }
    ensure!(
        container_changed,
        "history changed without advancing container date"
    );
    ensure!(
        number(b.at(&new[&pid], "date_modified")?)? > number(a.at(row, "date_modified")?)?,
        "history date did not advance"
    );
    ensure!(
        history_new.len() >= history_old.len(),
        "history queue shrank"
    );
    let first_new = history_new
        .first()
        .map(|r| num(bi, r, "container_item_pid"))
        .transpose()?;
    let dropped = match first_new {
        Some(id) => history_old
            .iter()
            .position(|r| num(ai, r, "container_item_pid").ok() == Some(id))
            .unwrap_or(history_old.len()),
        None => 0,
    };
    let retained = history_old.len() - dropped;
    ensure!(
        history_new.len() >= retained,
        "history lost retained suffix"
    );
    for (before, after) in history_old[dropped..].iter().zip(&history_new[..retained]) {
        budget.check()?;
        exact_except(ai, before, after, &["position"])?;
    }
    let mut appended = Vec::new();
    let mut appended_per_item = BTreeMap::<i64, u64>::new();
    for row in &history_new[retained..] {
        budget.check()?;
        let id = num(bi, row, "container_item_pid")?;
        let item = num(bi, row, "item_pid")?;
        ensure!(
            !all_old.contains_key(&id) && id > 0,
            "new history entry reuses an existing row identity"
        );
        // Only a track in the library can be played. Older entries may name
        // tracks removed since: a removal keeps their history rows (removal
        // batches), and the retained suffix is compared exactly above.
        ensure!(
            items.contains_key(&item),
            "new history entry references a foreign item"
        );
        // Playback History records starts/selections independently of completed
        // play counters. Do not borrow a counter increment or claim a one-to-one
        // mapping: ordinary_stats/check_revisions validate that channel separately.
        let count = appended_per_item.entry(item).or_default();
        *count = count
            .checked_add(1)
            .context("history event count overflow")?;
        for name in ["uuid", "position_uuid", "occurrence_id"] {
            ensure!(
                bi.at(row, name)?.text()?.is_empty(),
                "unknown new history relationship shape"
            );
        }
        appended.push(
            json!({"container_item_pid":id,"item_pid":item,"position":num(bi,row,"position")?}),
        );
    }
    Ok(History {
        changed_container: Some(pid),
        report: json!({"container_pid":pid,
        "before_date_modified":a.at(row,"date_modified")?,"after_date_modified":b.at(&new[&pid],"date_modified")?,
        "before_rows":history_old.len(),"after_rows":history_new.len(),"dropped_oldest":dropped,
            "retained_ordered_suffix":retained,"appended":appended,"appended_per_item":appended_per_item,
            "completed_play_attribution_claimed":false}),
    })
}

/// `floor`, when given, replaces the model's highest revision as the number
/// a renumbered or fresh revision must exceed.
fn check_revisions(
    floor: Option<i64>,
    a: &Table,
    b: &Table,
    played: &BTreeMap<i64, Played>,
    items: &BTreeMap<i64, Row>,
    history: Option<i64>,
    budget: &Budget,
) -> Result<Vec<Value>> {
    let keyed = |t: &Table| -> Result<BTreeMap<(i64, i64, i64), Row>> {
        let mut result = BTreeMap::new();
        for (row, count) in &t.rows {
            let key = (
                num(t, row, "entity_pid")?,
                num(t, row, "class")?,
                num(t, row, "revision_type")?,
            );
            ensure!(
                *count == 1 && result.insert(key, row.clone()).is_none(),
                "ambiguous entity revision"
            );
        }
        Ok(result)
    };
    let old = keyed(a)?;
    let new = keyed(b)?;
    let max_old = match floor {
        Some(floor) => floor,
        None => old
            .values()
            .map(|r| num(a, r, "revision"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0),
    };
    // Native housekeeping can advance a live item's full-record revision and
    // discard that item's older statistics revision. This is bookkeeping over
    // independently verified content, not a new import or proof of playback.
    let mut full_updates = BTreeMap::<i64, i64>::new();
    for (key, before) in &old {
        budget.check()?;
        let Some(after) = new.get(key) else { continue };
        if key.1 == 0 && key.2 == 0 && before != after {
            ensure!(
                items.contains_key(&key.0),
                "foreign full-record revision changed"
            );
            ensure!(
                num(a, before, "deleted")? == 0 && num(b, after, "deleted")? == 0,
                "full-record revision changed tombstone meaning"
            );
            exact_except(a, before, after, &["revision"])?;
            let revision = num(b, after, "revision")?;
            ensure!(
                revision > max_old,
                "full-record revision did not advance beyond prior generation"
            );
            full_updates.insert(key.0, revision);
        }
    }
    let mut fresh_stats = BTreeSet::new();
    let mut history_covered = false;
    let mut changes = Vec::new();
    for key in old
        .keys()
        .chain(new.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        budget.check()?;
        let before = old.get(&key);
        let after = new.get(&key);
        if before == after {
            continue;
        }
        let meaning = match (key.1, key.2) {
            (0, 0) => {
                ensure!(
                    before.is_some() && after.is_some() && full_updates.contains_key(&key.0),
                    "full-record revision identity added or removed"
                );
                "live_item_full_record_revision_advanced"
            }
            (0, 1) if after.is_none() => {
                let before = before.context("missing superseded statistics revision")?;
                ensure!(
                    items.contains_key(&key.0) && num(a, before, "deleted")? == 0,
                    "foreign or tombstoned statistics revision removed"
                );
                ensure!(
                    full_updates
                        .get(&key.0)
                        .is_some_and(|v| *v > num(a, before, "revision").unwrap_or(i64::MAX)),
                    "removed statistics revision lacks newer same-item full-record coverage"
                );
                "stats_revision_superseded_by_full_record"
            }
            (0, 1) => {
                ensure!(
                    played.contains_key(&key.0),
                    "unrelated statistics revision changed"
                );
                let after = after.context("missing statistics revision")?;
                let revision = num(b, after, "revision")?;
                ensure!(
                    num(b, after, "deleted")? == 0 && revision > max_old,
                    "statistics revision deleted or did not advance"
                );
                if let Some(before) = before {
                    exact_except(a, before, after, &["revision"])?;
                }
                if let Some(full) = new.get(&(key.0, 0, 0)) {
                    ensure!(
                        num(b, full, "deleted")? == 0 && revision > num(b, full, "revision")?,
                        "fresh statistics revision does not follow its full-record revision"
                    );
                }
                fresh_stats.insert(key.0);
                "completed_play_or_skip_statistics_revision_advanced"
            }
            (1, 0) if history == Some(key.0) => {
                let after = after.context("history revision removed")?;
                ensure!(
                    num(b, after, "deleted")? == 0 && num(b, after, "revision")? > max_old,
                    "history revision deleted or did not advance"
                );
                if let Some(before) = before {
                    exact_except(a, before, after, &["revision"])?;
                }
                history_covered = true;
                "playback_history_revision_advanced"
            }
            _ => anyhow::bail!("unrelated entity revision changed"),
        };
        changes.push(
            json!({"entity_pid":key.0,"class":key.1,"revision_type":key.2,
            "meaning":meaning,"before":before,"after":after}),
        );
    }
    ensure!(
        played
            .keys()
            .all(|id| fresh_stats.contains(id) || full_updates.contains_key(id)),
        "played item lacks fresh statistics or full-record revision coverage"
    );
    ensure!(
        history.is_none() || history_covered,
        "history lacks matching advanced entity revision"
    );
    Ok(changes)
}

#[cfg(test)]
mod tests;
