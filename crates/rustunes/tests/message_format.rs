//! `--message-format json` from the outside: offline commands run as a
//! frontend would run them, and every line they print is checked.
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// A scratch directory of this test's own.
fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "rustunes-message-format-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Run the CLI with `--message-format json`, isolated from this host's
/// settings and state.
fn run(args: &[&str], home: &Path, stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_rustunes"))
        .arg("--message-format")
        .arg("json")
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("USBMUXD_SOCKET_ADDRESS", home.join("no-usbmuxd"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Every line a record, numbered from 0, from `start` to `result`, and
/// nothing on stderr.
fn checked(output: &Output) -> Vec<Value> {
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(text.ends_with('\n'), "{text}");
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    for (seq, record) in records.iter().enumerate() {
        assert_eq!(record["seq"], seq, "{record}");
        assert!(record["type"].is_string(), "{record}");
        assert!(record["elapsed_ms"].is_u64(), "{record}");
    }
    assert_eq!(records[0]["type"], "start");
    assert_eq!(records[0]["schema"], "rustunes.messages");
    let result = records.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["exit_code"], output.status.code().unwrap());
    assert_eq!(result["ok"], output.status.success());
    records
}

fn of<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
    records.iter().filter(|r| r["type"] == kind).collect()
}

fn report<'a>(records: &'a [Value], kind: &str) -> &'a Value {
    let reports: Vec<_> = of(records, "report")
        .into_iter()
        .filter(|r| r["kind"] == kind)
        .collect();
    assert_eq!(reports.len(), 1, "{records:#?}");
    &reports[0]["report"]
}

#[test]
fn inspect_reports_the_source_as_inspect_json_does() {
    let home = scratch("inspect");
    let file = fixtures().join("tone-alac.m4a");
    let output = run(&["inspect", file.to_str().unwrap()], &home, "");
    let records = checked(&output);
    assert!(output.status.success());
    assert_eq!(records[0]["command"], "inspect");
    assert_eq!(records[0]["options"]["file"], file.to_str().unwrap());
    let source = report(&records, "source");
    assert_eq!(source["codec"], "alac");
    assert_eq!(records.last().unwrap()["outcome"], "completed");
}

#[test]
fn validate_and_scan_report_their_json() {
    let home = scratch("validate");
    let file = fixtures().join("tone-mp3-vbr.mp3");
    let output = run(&["validate", file.to_str().unwrap()], &home, "");
    let records = checked(&output);
    let validation = report(&records, "validation");
    assert!(validation["supported_by_current_profile"].is_boolean());

    let output = run(&["scan", fixtures().to_str().unwrap()], &home, "");
    let records = checked(&output);
    let scan = report(&records, "scan");
    assert!(
        scan["selected"].as_array().is_some_and(|s| !s.is_empty()),
        "{scan}"
    );
    let progress = of(&records, "progress");
    assert!(!progress.is_empty());
    assert!(progress.iter().all(|p| p["phase"] == "inspect_sources"));
    // The file in hand is named, and bytes lead the estimate.
    assert!(
        progress.iter().any(|p| p["current"]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with(".m4a"))),
        "{progress:#?}"
    );
    let last = progress.last().unwrap();
    assert_eq!(last["done"], last["total"]);
    assert_eq!(last["percent"], 100.0);
    assert_eq!(last["bytes_done"], last["bytes_total"]);
    // A phase record comes before the progress of its phase.
    let phase = records.iter().position(|r| r["type"] == "phase").unwrap();
    let first = records
        .iter()
        .position(|r| r["type"] == "progress")
        .unwrap();
    assert!(phase < first);
}

#[test]
fn config_reports_the_library_it_sets() {
    let home = scratch("config");
    let output = run(&["config", "library"], &home, "");
    let records = checked(&output);
    let config = report(&records, "config");
    assert_eq!(config["library"], Value::Null);
    assert_eq!(config["changed"], false);

    let library = home.join("music");
    std::fs::create_dir(&library).unwrap();
    let output = run(&["config", "library", library.to_str().unwrap()], &home, "");
    let records = checked(&output);
    let config = report(&records, "config");
    assert_eq!(config["changed"], true);
    assert_eq!(
        config["library"],
        std::fs::canonicalize(&library).unwrap().to_str().unwrap()
    );
}

#[test]
fn cache_and_completions_report_their_output() {
    let home = scratch("cache");
    let output = run(&["cache", "stats"], &home, "");
    let records = checked(&output);
    assert_eq!(report(&records, "cache_statistics")["entries"], 0);

    let output = run(&["cache", "prune", "--dry-run"], &home, "");
    let records = checked(&output);
    assert_eq!(report(&records, "cache_prune")["removed"], 0);
    assert_eq!(records.last().unwrap()["outcome"], "dry_run");

    let output = run(&["completions", "bash"], &home, "");
    let records = checked(&output);
    let completions = report(&records, "completions");
    assert!(
        completions["script"]
            .as_str()
            .is_some_and(|s| s.contains("--message-format"))
    );
}

#[test]
fn an_error_is_the_result_and_nothing_else() {
    let home = scratch("error");
    let missing = home.join("missing.m4a");
    let output = run(&["inspect", missing.to_str().unwrap()], &home, "");
    assert_eq!(output.status.code(), Some(1));
    let records = checked(&output);
    assert_eq!(records.len(), 2, "{records:#?}");
    let result = &records[1];
    assert_eq!(result["outcome"], "failed");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
    assert!(result["error"]["causes"].is_array());
}

#[test]
fn a_command_line_clap_refuses_is_a_usage_error() {
    let home = scratch("usage");
    let output = run(&["no-such-command"], &home, "");
    assert_eq!(output.status.code(), Some(2));
    let records = checked(&output);
    let result = records.last().unwrap();
    assert_eq!(result["outcome"], "usage_error");
    assert!(
        result["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("no-such-command"))
    );
}

#[test]
fn a_command_that_needs_the_phone_fails_as_a_record_without_one() {
    let home = scratch("phone");
    let output = run(&["device", "info"], &home, "");
    assert_eq!(output.status.code(), Some(1));
    let records = checked(&output);
    assert_eq!(records.last().unwrap()["outcome"], "failed");
}
