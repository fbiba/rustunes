//! One observed clock property in an otherwise exact ordinary recovery scope.
//! This evidence does not grant transport or journal completion authority.
use super::{Budget, Row, Table};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const KEY: &str = "MLMaintenanceTaskOperationDate";

pub(super) fn check(before: &Table, after: &Table, budget: &Budget) -> Result<Option<Value>> {
    budget.check()?;
    if before.rows == after.rows {
        return Ok(None);
    }
    ensure!(
        before.columns == ["key", "value"] && after.columns == before.columns,
        "unsupported maintenance property columns"
    );
    let old = date_row(before, budget)?;
    let new = date_row(after, budget)?;
    let mut old_other = before.rows.clone();
    let mut new_other = after.rows.clone();
    old_other.remove(old);
    new_other.remove(new);
    ensure!(
        old_other == new_other,
        "non-maintenance database property changed"
    );
    let old_text = before.at(old, "value")?.text()?;
    let new_text = after.at(new, "value")?.text()?;
    let old_date = decimal_date(old_text)?;
    let new_date = decimal_date(new_text)?;
    ensure!(new_date > old_date, "maintenance date did not advance");
    budget.check()?;
    Ok(Some(json!({"before": old_text, "after": new_text})))
}

fn date_row<'a>(table: &'a Table, budget: &Budget) -> Result<&'a Row> {
    let mut selected = None;
    for (row, count) in &table.rows {
        budget.check()?;
        if table.at(row, "key")?.text()? == KEY {
            ensure!(
                *count == 1 && selected.is_none(),
                "ambiguous maintenance date"
            );
            selected = Some(row);
        }
    }
    selected.context("maintenance date added or removed")
}

fn decimal_date(value: &str) -> Result<f64> {
    let parts: Vec<_> = value.split('.').collect();
    ensure!(
        parts.len() <= 2
            && parts
                .iter()
                .all(|part| { !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) }),
        "maintenance date is not nonnegative decimal text"
    );
    let date: f64 = value.parse().context("invalid maintenance date")?;
    ensure!(date.is_finite() && date >= 0.0, "invalid maintenance date");
    Ok(date)
}
