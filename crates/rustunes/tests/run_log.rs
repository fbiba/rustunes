//! The log every run keeps for bug reports, from the outside: where it is,
//! what it holds, and what a failure prints about it.
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A scratch home of this test's own.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("rustunes-run-log-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Run the CLI isolated from this host's settings and state; no usbmuxd
/// answers, so every command that reaches the phone fails. The socket path
/// is short, as one under a long temporary directory fails another way.
fn run(args: &[&str], home: &Path, log_filter: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rustunes"));
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env(
            "USBMUXD_SOCKET_ADDRESS",
            "/nonexistent/rustunes-test-usbmuxd",
        )
        .env_remove("RUSTUNES_LOG");
    if let Some(filter) = log_filter {
        command.env("RUSTUNES_LOG", filter);
    }
    command.output().unwrap()
}

/// The only log in `home`'s state directory, which must be private.
fn only_log(home: &Path) -> (PathBuf, String) {
    let logs: Vec<_> = std::fs::read_dir(home.join("state/rustunes/logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let mode = logs[0].metadata().unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&logs[0]).unwrap();
    (logs[0].clone(), text)
}

/// The log `logs/failed` keeps of a failed run: the same file as `log`.
fn kept(log: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let kept = log
        .parent()
        .unwrap()
        .join("failed")
        .join(log.file_name().unwrap());
    assert_eq!(
        kept.metadata().unwrap().ino(),
        log.metadata().unwrap().ino()
    );
    kept
}

#[test]
fn a_failure_prints_its_error_what_the_log_adds_and_where_it_is_kept() {
    let home = scratch("failure");
    let output = run(&["device", "info"], &home, None);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (path, log) = only_log(&home);
    assert!(
        stderr.starts_with("Error: connecting to usbmuxd, which is not running"),
        "{stderr}"
    );
    let (error, added) = stderr
        .split_once("\nLast lines of the log not shown above:\n")
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(error.contains("Caused by:"), "{stderr}");
    assert!(
        added.contains("  INFO rustunes: Command line: "),
        "{stderr}"
    );
    assert!(!added.contains("connecting to usbmuxd"), "{stderr}");
    assert!(
        added.ends_with(&format!("Log of this run: {}\n", kept(&path).display())),
        "{stderr}"
    );
    assert!(
        log.contains(&format!("rustunes {} on ", env!("CARGO_PKG_VERSION"))),
        "{log}"
    );
    assert!(
        log.contains(r#"Command line: [""#) && log.contains(r#""device", "info"]"#),
        "{log}"
    );
    assert!(
        log.contains(" ERROR rustunes::shown: connecting to usbmuxd, which is not running"),
        "{log}"
    );
    assert!(log.contains("Caused by:"), "{log}");
}

#[test]
fn json_output_names_the_log_in_its_start_record_and_leaves_stderr_empty() {
    let home = scratch("json");
    let output = run(&["--message-format", "json", "device", "info"], &home, None);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let start: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    let (path, log) = only_log(&home);
    assert_eq!(start["log"], path.to_str().unwrap());
    kept(&path);
    assert!(
        log.contains(" ERROR rustunes::shown: connecting to usbmuxd, which is not running"),
        "{log}"
    );
}

#[test]
fn verbose_prints_the_log_on_stderr_but_not_the_lines_already_shown() {
    let home = scratch("verbose");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let output = run(&["-v", "scan", fixture.to_str().unwrap()], &home, None);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (_, log) = only_log(&home);
    assert!(!home.join("state/rustunes/logs/failed").exists());
    assert!(stderr.contains("Command line: "), "{stderr}");
    assert!(stderr.contains("Finished with exit code 0"), "{stderr}");
    // A notice is in the log, and on stderr only as itself.
    let shown = "Source cache: ";
    assert!(log.contains(shown), "{log}");
    assert!(
        stderr
            .lines()
            .filter(|line| line.contains(shown))
            .all(|line| line.starts_with(shown)),
        "{stderr}"
    );
}

#[test]
fn a_verbose_failure_does_not_print_the_log_lines_again() {
    let home = scratch("verbose-failure");
    let output = run(&["-v", "device", "info"], &home, None);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let (path, _) = only_log(&home);
    assert_eq!(stderr.matches("Command line: ").count(), 1, "{stderr}");
    assert!(!stderr.contains("Last lines of the log"), "{stderr}");
    assert!(
        stderr.ends_with(&format!("Log of this run: {}\n", kept(&path).display())),
        "{stderr}"
    );
}

#[test]
fn an_unreadable_rustunes_log_fails_the_run_without_a_log() {
    let home = scratch("filter");
    let output = run(&["config", "library"], &home, Some("rustunes=loud"));
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("Error: RUSTUNES_LOG must be a log filter"),
        "{stderr}"
    );
    assert!(!stderr.contains("Log of this run"), "{stderr}");
    assert!(!home.join("state/rustunes/logs").exists());
}
