use super::*;
use std::env::VarError;
use tracing::Level;

fn unset() -> Result<String, VarError> {
    Err(VarError::NotPresent)
}

#[test]
fn verbosity_raises_what_the_libraries_log_but_not_afc_bytes_before_everything() {
    let default = filter(0, unset()).unwrap();
    assert!(default.would_enable("rustunes::cli", &Level::DEBUG));
    assert!(default.would_enable("idevice::lockdown", &Level::INFO));
    assert!(!default.would_enable("idevice::lockdown", &Level::DEBUG));
    assert_eq!(filter(1, unset()).unwrap(), default);

    let libraries = filter(2, unset()).unwrap();
    assert!(libraries.would_enable("idevice::lockdown", &Level::DEBUG));
    assert!(!libraries.would_enable("idevice::services::afc::packet", &Level::DEBUG));
    assert!(!libraries.would_enable("idevice::lockdown", &Level::TRACE));

    let everything = filter(3, unset()).unwrap();
    assert!(everything.would_enable("idevice::services::afc::packet", &Level::TRACE));
}

#[test]
fn rustunes_log_replaces_the_filter_and_an_unreadable_one_fails() {
    let asked = filter(3, Ok("idevice=debug".into())).unwrap();
    assert!(asked.would_enable("idevice::usbmuxd", &Level::DEBUG));
    assert!(!asked.would_enable("rustunes::cli", &Level::ERROR));
    assert_eq!(
        filter(0, Ok(String::new())).unwrap(),
        filter(0, unset()).unwrap()
    );
    for refused in [
        Ok("idevice=loud".into()),
        Err(VarError::NotUnicode("\u{fffd}".into())),
    ] {
        let error = filter(0, refused).unwrap_err();
        assert!(format!("{error:#}").contains("RUSTUNES_LOG"), "{error:#}");
    }
}

#[test]
fn stderr_leaves_out_the_lines_the_terminal_shows() {
    let stderr = filter(1, unset())
        .unwrap()
        .with_target(SHOWN, LevelFilter::OFF);
    assert!(!stderr.would_enable(SHOWN, &Level::WARN));
    assert!(stderr.would_enable("rustunes::engine", &Level::DEBUG));
}

#[test]
fn pruning_keeps_the_newest_logs_and_files_it_did_not_name() {
    let logs = std::env::temp_dir().join(format!("rustunes-logging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&logs);
    fs::create_dir_all(&logs).unwrap();
    // Numerically, not lexically, ordered: 9 is older than 10.
    for timestamp in 9..9 + KEPT as u128 + 5 {
        fs::write(logs.join(format!("{timestamp}-42.log")), "").unwrap();
    }
    for foreign in ["notes.log", "1-2.txt", "x-1.log", "1-x.log"] {
        fs::write(logs.join(foreign), "").unwrap();
    }
    fs::create_dir(logs.join("3-4.log")).unwrap();
    prune(&logs);
    let mut left: Vec<String> = fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    let mut expected: Vec<String> = (9 + 6..9 + KEPT as u128 + 5)
        .map(|timestamp| format!("{timestamp}-42.log"))
        .chain(["notes.log", "1-2.txt", "x-1.log", "1-x.log", "3-4.log"].map(String::from))
        .collect();
    expected.sort();
    assert_eq!(left, expected);
    assert_eq!(left.len(), KEPT - 1 + 5);
    fs::remove_dir_all(&logs).unwrap();
}

#[test]
fn per_chunk_progress_and_a_cached_pass_are_thinned() {
    let fingerprinted = |completed| {
        Event::Observation(Observation::AudioFingerprinted {
            track: 1,
            completed,
            total: 120,
            bytes: 0,
            other_codec: false,
            reused: true,
        })
    };
    assert!(floods(&fingerprinted(49)));
    assert!(!floods(&fingerprinted(50)));
    assert!(!floods(&fingerprinted(120)));
    assert!(floods(&Event::Observation(
        Observation::CopyingTrackAudio {
            track: 1,
            title: "One",
            artist: "A",
            read: 1,
            file_bytes: 2,
        }
    )));
    assert!(!floods(&Event::Observation(
        Observation::CapturingCatalogue
    )));
}

#[test]
fn a_failure_prints_only_the_last_lines_the_terminal_did_not_show() {
    use std::io::Write;
    for number in 1..=RECENT + 5 {
        let mut line = Recent(Vec::new());
        writeln!(line, "line {number}").unwrap();
    }
    // An empty line is not kept.
    drop(Recent(Vec::new()));
    let recent = RECENT_LINES.lock().unwrap().clone();
    assert_eq!(recent.len(), RECENT);
    assert_eq!(recent.front().unwrap(), "line 6");
    assert_eq!(recent.back().unwrap(), &format!("line {}", RECENT + 5));
}
