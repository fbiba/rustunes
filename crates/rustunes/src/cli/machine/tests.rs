use super::{Progress, State, error_json, operation, record, requested};
use clap::CommandFactory;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn args(line: &str) -> Vec<std::ffi::OsString> {
    line.split(' ').map(Into::into).collect()
}

#[test]
fn records_lead_with_their_envelope_on_one_line() {
    let line = record("notice", 7, 1234, json!({"message": "a\nb", "code": "x"})).unwrap();
    assert_eq!(
        line,
        "{\"type\":\"notice\",\"seq\":7,\"elapsed_ms\":1234,\"code\":\"x\",\"message\":\"a\\nb\"}\n"
    );
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["message"], "a\nb");
}

#[test]
fn the_flag_is_found_before_clap_parses_it() {
    assert!(requested(&args("rustunes --message-format json sync")));
    assert!(requested(&args(
        "rustunes sync --message-format=json --bogus"
    )));
    assert!(!requested(&args("rustunes sync --message-format human")));
    assert!(!requested(&args("rustunes push -- --message-format=json")));
    assert!(!requested(&args("rustunes --message-format")));
}

#[test]
fn the_operation_names_the_subcommand_and_the_options_given() {
    let mut command = crate::cli::Cli::command();
    command.build();
    let matches = command
        .clone()
        .try_get_matches_from(args(
            "rustunes --wifi-address 192.0.2.5 push /music --batch-size 5 --include a --include b --dry-run --message-format json",
        ))
        .unwrap();
    let (name, options) = operation(&command, &matches);
    assert_eq!(name, "push");
    assert_eq!(
        options,
        json!({
            "wifi_address": "192.0.2.5",
            "path": "/music",
            "batch_size": "5",
            "include": ["a", "b"],
            "dry_run": true,
            "message_format": "json",
        })
    );
    let matches = command
        .clone()
        .try_get_matches_from(args("rustunes library list --artist A --json"))
        .unwrap();
    let (name, options) = operation(&command, &matches);
    assert_eq!(name, "library list");
    assert_eq!(options, json!({"artist": "A", "json": true}));
    let matches = command
        .clone()
        .try_get_matches_from(args("rustunes delete 00000000000000aa"))
        .unwrap();
    assert_eq!(
        operation(&command, &matches).1,
        json!({"ids": ["00000000000000aa"]})
    );
}

#[test]
fn a_stream_is_throttled_between_its_first_and_last_record() {
    let mut state = State::default();
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let update = |done| Progress::new("insert", "tracks", done, Some(10));
    assert!(state.advance(update(0), at(0)).is_some());
    assert!(state.advance(update(1), at(100)).is_none());
    assert!(state.advance(update(2), at(300)).is_some());
    assert!(state.advance(update(3), at(400)).is_none());
    assert!(
        state.advance(update(10), at(410)).is_some(),
        "the last record"
    );
    // A new pass of the stream is written at once.
    assert!(state.advance(update(0), at(420)).is_some());
}

#[test]
fn estimates_follow_the_average_rate_since_the_pass_began() {
    let mut state = State::default();
    let t0 = Instant::now();
    let first = state
        .advance(Progress::new("add", "tracks", 0, Some(10)), t0)
        .unwrap();
    assert_eq!(first["percent"], 0.0);
    assert_eq!(first["eta_ms"], Value::Null, "no rate yet");
    let later = state
        .advance(
            Progress::new("add", "tracks", 4, Some(10)).current(json!({"path": "a.m4a"})),
            t0 + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(later["percent"], 40.0);
    assert_eq!(later["rate_per_second"], 2.0);
    assert_eq!(later["eta_ms"], 3000);
    assert_eq!(later["current"]["path"], "a.m4a");
    assert!(later.get("bytes_done").is_none());
}

#[test]
fn bytes_lead_the_estimate_and_count_only_updates_keep_them() {
    let mut state = State::default();
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let bytes = |done, total, b| {
        Progress::new("inspect_sources", "files", done, Some(total)).bytes(b, Some(100))
    };
    state.advance(bytes(0, 4, 0), at(0)).unwrap();
    let one = state.advance(bytes(1, 4, 25), at(1000)).unwrap();
    assert_eq!(one["percent"], 25.0, "by bytes, not by files");
    assert_eq!(one["eta_ms"], 3000);
    assert_eq!(one["bytes_per_second"], 25.0);
    // A count alone keeps the stream's bytes, and completes them with it.
    let counted = state
        .advance(
            Progress::new("inspect_sources", "files", 2, Some(4)),
            at(1300),
        )
        .unwrap();
    assert_eq!(counted["bytes_done"], 25);
    assert_eq!(counted["bytes_total"], 100);
    let done = state
        .advance(
            Progress::new("inspect_sources", "files", 4, Some(4)),
            at(1400),
        )
        .unwrap();
    assert_eq!(done["bytes_done"], 100);
    assert_eq!(done["percent"], 100.0);
    assert_eq!(done["eta_ms"], 0);
}

#[test]
fn bytes_without_a_total_keep_the_streams_total() {
    let mut state = State::default();
    let t0 = Instant::now();
    state
        .advance(
            Progress::new("index_audio", "tracks", 0, Some(2)).bytes(0, Some(80)),
            t0,
        )
        .unwrap();
    let copied = state
        .advance(
            Progress::new("index_audio", "tracks", 0, Some(2)).bytes(20, None),
            t0 + Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(copied["bytes_total"], 80);
    assert_eq!(copied["percent"], 25.0);
    assert_eq!(copied["eta_ms"], 3000);
}

#[test]
fn an_unknown_total_has_no_percent_or_estimate() {
    let mut state = State::default();
    let t0 = Instant::now();
    state
        .advance(Progress::new("receive", "frames", 1, None), t0)
        .unwrap();
    let later = state
        .advance(
            Progress::new("receive", "frames", 3, None),
            t0 + Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(later["total"], Value::Null);
    assert_eq!(later["percent"], Value::Null);
    assert_eq!(later["eta_ms"], Value::Null);
    assert_eq!(later["rate_per_second"], 1.0);
}

#[test]
fn errors_carry_their_causes_in_order() {
    let error = anyhow::anyhow!("socket closed")
        .context("reading the catalogue")
        .context("snapshot failed");
    assert_eq!(
        error_json(&error),
        json!({"message": "snapshot failed", "causes": ["reading the catalogue", "socket closed"]})
    );
}

#[test]
fn data_is_base64() {
    for (bytes, text) in [
        (&b""[..], ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"\xff\xfe\x00", "//4A"),
    ] {
        assert_eq!(crate::cli::base64(bytes), text);
    }
}

#[test]
fn a_stream_without_bytes_counts_the_current_items_part() {
    let mut state = State::default();
    let t0 = Instant::now();
    state
        .advance(Progress::new("add", "tracks", 0, Some(2)), t0)
        .unwrap();
    let uploading = state
        .advance(
            Progress::new("add", "tracks", 0, Some(2))
                .current(json!({"path": "a.m4a", "bytes_done": 50, "bytes_total": 100})),
            t0 + Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(uploading["percent"], 25.0);
    assert_eq!(uploading["rate_per_second"], 0.5);
    assert_eq!(uploading["eta_ms"], 3000);
    assert!(
        uploading.get("bytes_done").is_none(),
        "the stream has no bytes"
    );
}

#[test]
fn a_new_pass_inherits_nothing_and_a_finished_one_is_written_once() {
    let mut state = State::default();
    let t0 = Instant::now();
    let at = |ms| t0 + Duration::from_millis(ms);
    let files = |done, total| Progress::new("inspect_sources", "files", done, Some(total));
    state.advance(files(0, 4), at(0)).unwrap();
    // Gaining a byte total is written at once, in the same pass.
    assert!(
        state
            .advance(files(1, 4).bytes(0, Some(100)), at(10))
            .is_some()
    );
    assert!(
        state
            .advance(files(2, 4).bytes(50, Some(100)), at(15))
            .is_none()
    );
    let done = state.advance(files(4, 4), at(20)).unwrap();
    assert_eq!(done["bytes_done"], 100);
    assert!(
        state
            .advance(files(4, 4).bytes(100, Some(100)), at(30))
            .is_none()
    );
    // A second pass, of an empty directory, has no bytes of the first.
    let empty = state.advance(files(0, 0), at(40)).unwrap();
    assert!(empty.get("bytes_done").is_none(), "{empty}");
    assert_eq!(empty["percent"], 100.0);
    assert!(
        state
            .advance(files(0, 0).bytes(0, Some(0)), at(50))
            .is_none()
    );
}

#[test]
fn the_sync_report_lists_each_file_left_out_and_when() {
    let result = crate::cli::progress::tests::skipping(crate::engine::SyncOutcome::Completed);
    let report = super::events::sync_json(&result);
    assert_eq!(report["skipped_count"], 2);
    assert_eq!(
        report["skipped"],
        serde_json::json!([
            {"path": "Artist A - \"One\".flac", "code": "mp3_source_profile_unsupported",
             "reason": result.skipped[0].reason, "detail": result.skipped[0].detail, "stage": "plan"},
            {"path": "Artist B - Two.flac", "code": "flac_conversion_failed",
             "reason": result.skipped[1].reason, "detail": result.skipped[1].detail, "stage": "send"},
        ])
    );
    // Exactly the keys docs/07 lists for the report.
    let keys: std::collections::BTreeSet<_> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "outcome",
            "run",
            "plan",
            "journal",
            "managed_covers_verified",
            "existing_unmanaged_covers_unverified",
            "metadata_updates_verified",
            "covers_left_out",
            "skipped",
            "skipped_count",
            "message",
        ])
    );
}
