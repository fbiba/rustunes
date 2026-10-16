//! Independent completed-play/skip channel checks for the playback bridge,
//! used by both the insert and the cover recovery routes.
use super::*;

const FIELDS: [&str; 7] = [
    "play_count_user",
    "play_count_recent",
    "has_been_played",
    "date_played",
    "skip_count_user",
    "skip_count_recent",
    "date_skipped",
];

pub(super) fn check(
    a: &Table,
    b: &Table,
    items: &BTreeMap<i64, Row>,
    budget: &Budget,
) -> Result<BTreeMap<i64, Played>> {
    budget.check()?;
    let old = keyed(a, "item_pid")?;
    let new = keyed(b, "item_pid")?;
    ensure!(
        old.keys().eq(new.keys()),
        "ordinary playback stats membership changed"
    );
    let mut changed = BTreeMap::new();
    for (pid, before) in &old {
        budget.check()?;
        let after = &new[pid];
        if before == after {
            continue;
        }
        ensure!(
            items.contains_key(pid),
            "ordinary playback references a foreign item"
        );
        exact_except(a, before, after, &FIELDS)?;
        let delta = |field: &str| -> Result<u64> {
            let x = num(a, before, field)?;
            let y = num(b, after, field)?;
            ensure!(
                (0..=i64::from(u32::MAX)).contains(&x) && (x..=i64::from(u32::MAX)).contains(&y),
                "ordinary playback counter decreased or exceeds native range: {field}"
            );
            Ok(u64::try_from(y - x)?)
        };
        let plays = delta("play_count_user")?;
        let skips = delta("skip_count_user")?;
        ensure!(
            plays == delta("play_count_recent")?
                && skips == delta("skip_count_recent")?
                && (plays > 0 || skips > 0),
            "ordinary play/skip increments differ or no completed event exists"
        );
        let old_played = num(a, before, "has_been_played")?;
        let new_played = num(b, after, "has_been_played")?;
        ensure!(
            (0..=1).contains(&old_played) && new_played == if plays > 0 { 1 } else { old_played },
            "ordinary has_been_played is not explained by completed plays"
        );
        for (field, count) in [("date_played", plays), ("date_skipped", skips)] {
            let old_date = a.at(before, field)?;
            let new_date = b.at(after, field)?;
            if count == 0 {
                ensure!(
                    old_date == new_date,
                    "ordinary {field} changed without its event"
                );
            } else {
                ensure!(
                    number(new_date)? > number(old_date)?,
                    "ordinary {field} did not advance"
                );
            }
        }
        let fields = |t: &Table, r: &Row| -> Result<BTreeMap<String, Cell>> {
            FIELDS
                .iter()
                .map(|key| Ok(((*key).into(), t.at(r, key)?.clone())))
                .collect()
        };
        changed.insert(
            *pid,
            Played {
                item_pid: *pid,
                play_count_delta: plays,
                skip_count_delta: skips,
                before: fields(a, before)?,
                after: fields(b, after)?,
            },
        );
    }
    Ok(changed)
}
