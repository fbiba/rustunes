//! Presentation of the local fingerprint cache; no device connection.
use crate::engine::state::audio_index::cache::Statistics;
use anyhow::Result;
use serde_json::json;
use std::io::Write;

pub fn statistics(report: &Statistics, json: bool, output: &mut impl Write) -> Result<()> {
    if json {
        serde_json::to_writer_pretty(&mut *output, report)?;
        writeln!(output)?;
        return Ok(());
    }
    writeln!(output, "Fingerprint cache: {}", report.database.display())?;
    if report.entries == 0 && report.database_bytes == 0 {
        writeln!(
            output,
            "No entries yet; the next index pass copies each file once."
        )?;
        return Ok(());
    }
    writeln!(
        output,
        "{} entries, {} bytes of entries in a {}-byte database",
        report.entries, report.entry_bytes, report.database_bytes
    )?;
    let lookups = report.hits + report.writes;
    if lookups > 0 {
        writeln!(
            output,
            "Reused {} times and copied {} times since the entries were stored ({:.1}% reused)",
            report.hits,
            report.writes,
            report.hits as f64 * 100.0 / lookups as f64
        )?;
    }
    if let (Some(oldest), Some(newest)) =
        (report.oldest_last_seen_unix, report.newest_last_seen_unix)
    {
        writeln!(
            output,
            "Last used between {} and {}",
            date(oldest),
            date(newest)
        )?;
    }
    writeln!(
        output,
        "{} entries unused for {} days; `cache prune --unseen-days {}` removes them",
        report.unseen, report.unseen_days, report.unseen_days
    )?;
    Ok(())
}

pub fn prune(
    removed: u64,
    unseen_days: u64,
    dry_run: bool,
    json: bool,
    output: &mut impl Write,
) -> Result<()> {
    if json {
        serde_json::to_writer_pretty(
            &mut *output,
            &json!({"unseen_days":unseen_days,"dry_run":dry_run,"removed":removed}),
        )?;
        writeln!(output)?;
    } else if dry_run {
        writeln!(
            output,
            "Would remove {removed} entries unused for {unseen_days} days."
        )?;
    } else {
        writeln!(
            output,
            "Removed {removed} entries unused for {unseen_days} days. A removed file is copied again if an index pass sees it."
        )?;
    }
    Ok(())
}

/// UTC calendar date of a Unix time, without a date library.
fn date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(951_782_400), "2000-02-29");
        assert_eq!(super::date(1_718_023_843), "2024-06-10");
    }
}
