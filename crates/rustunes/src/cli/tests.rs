use super::{Cli, Command, relevant_syslog};
use clap::{CommandFactory, Parser};

#[test]
fn mpeg_keep_text_reports_matched_ownership() {
    for (ownership, expected) in [
        ("inserted_by_us", "ownership: inserted_by_us"),
        ("unowned", "ownership: unowned"),
    ] {
        let matches = [crate::engine::planning::additive::ExistingMatch {
            track_id: "0000000000000001".into(),
            wire_id: "0000000000000001".into(),
            path: "iTunes_Control/Music/F00/TEST.mp3".into(),
            codec: "mpeg1_layer3",
            encoded_audio_equal: true,
            full_container_equal: true,
            match_basis: Some("full_file_equality_with_mpeg_layout"),
            ownership,
            catalog_differences: vec![],
            canonicalized_fields: vec![],
            device_file_sha256: None,
        }];
        let mut out = Vec::new();
        super::write_mpeg_keep(&mut out, "track.mp3", &matches).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("catalogue metadata and artwork preserved"));
        assert_eq!(matches[0].ownership, ownership);
        if ownership != "unowned" {
            assert!(!text.contains("unowned"));
        }
    }
}

#[test]
fn mpeg_keep_text_never_invents_missing_ownership() {
    let mut out = Vec::new();
    super::write_mpeg_keep(&mut out, "track.mp3", &[]).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("ownership: not established"));
    assert!(!text.contains("unowned"));
}

#[test]
fn nested_help_and_completions_need_no_local_state() {
    for args in [
        vec!["tool", "cache", "--help"],
        vec!["tool", "cache", "prune", "--help"],
    ] {
        let help = Cli::try_parse_from(args).err().unwrap();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    }
    let mut output = Vec::new();
    super::completions::bash(Cli::command(), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("'cache:prune') key='cache/prune'"));
    let prune_options = output
        .lines()
        .find(|line| line.contains("'cache/prune') opts="))
        .unwrap();
    for name in ["--dry-run", "--unseen-days", "--json", "--device"] {
        assert!(prune_options.contains(name), "{name}");
    }
}

#[test]
fn artwork_plan_text_discloses_both_recent_counters() {
    for play in [0, 4] {
        let plan = crate::engine::planning::additive::tests::artwork_disclosure_test_plan(play, 3);
        let mut out = Vec::new();
        super::write_artwork_recent_disclosure(&mut out, &plan).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("recent-skip counters on 1 tracks"), "{text}");
        assert!(text.contains("RECENT SKIP"), "{text}");
        assert!(text.contains("3 -> 0"), "{text}");
        assert!(
            text.contains("both recent counters unchanged or reset all to zero"),
            "{text}"
        );
        assert!(text.contains("RECENT PLAY") == (play > 0), "{text}");
    }
}

#[test]
fn artwork_plan_text_play_only_disclosure_and_zero_silence() {
    let plan = crate::engine::planning::additive::tests::artwork_disclosure_test_plan(4, 0);
    let mut out = Vec::new();
    super::write_artwork_recent_disclosure(&mut out, &plan).unwrap();
    assert_eq!(out,b"Artwork import may reset recent-play counters on 1 tracks; lifetime counts must stay unchanged.\nRECENT PLAY \"ffffffffffffffff\": 4 -> 0\n");
    let plan = crate::engine::planning::additive::tests::artwork_disclosure_test_plan(0, 0);
    let mut out = Vec::new();
    super::write_artwork_recent_disclosure(&mut out, &plan).unwrap();
    assert!(out.is_empty());
}

#[test]
fn removal_cli_accepts_explicit_confirmation_and_preserves_full_prune_scope() {
    assert!(matches!(
        Cli::try_parse_from([
            "tool",
            "--device",
            "chosen-device",
            "delete",
            "0123456789abcdef",
            "--snapshot",
            "saved",
            "--journal",
            "journal",
            "--yes",
            "--json",
        ])
        .unwrap()
        .command,
        Command::Delete { .. }
    ));
    for command in ["delete", "prune", "clear"] {
        let mut args = vec!["tool", command];
        match command {
            "delete" => args.push("0123456789abcdef"),
            "prune" => args.push("music"),
            _ => {}
        }
        args.extend(["--dry-run", "--snapshot", "saved", "--output", "plan.json"]);
        assert!(Cli::try_parse_from(&args).is_ok(), "{command}");
        args.extend(["--seconds", "1801"]);
        assert!(Cli::try_parse_from(&args).is_err(), "{command}");
    }
    for flag in ["--include", "--exclude"] {
        assert!(Cli::try_parse_from(["tool", "prune", "music", flag, "partial"]).is_err());
    }
    for flag in ["--no-recursive", "--include-backups"] {
        assert!(Cli::try_parse_from(["tool", "prune", "music", flag]).is_err());
    }
    assert!(Cli::try_parse_from(["tool", "prune", "music", "--recursive"]).is_ok());
}

#[tokio::test]
async fn deletion_id_errors_remain_runtime_errors_before_engine_effects() {
    let engine = crate::engine::Engine::new(
        crate::engine::Config {
            device: crate::engine::DeviceConfig::default(),
            deadline: None,
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        |_| panic!("invalid deletion IDs must fail before engine effects"),
    );
    for (id, message) in [
        ("invalid", "track IDs require exactly 16 hexadecimal digits"),
        ("0000000000000000", "zero or duplicate track ID"),
    ] {
        for json in [false, true] {
            let mut args = vec!["tool", "delete", id, "--dry-run"];
            if json {
                args.push("--json");
            }
            let cli = Cli::try_parse_from(args).unwrap();
            let error = super::dispatch(cli, &engine).await.unwrap_err();
            assert_eq!(error.to_string(), message);
        }
    }
}

#[test]
fn track_number_cli_is_explicit_and_bounded() {
    let base = [
        "tool",
        "metadata",
        "set",
        "ffffffffffffffff",
        "--snapshot",
        "saved",
        "--journal",
        "journal",
    ];
    assert!(Cli::try_parse_from(base).is_err());
    for number in ["65536", "-1", "1.5"] {
        let mut args = base.to_vec();
        args.extend(["--track-number", number, "--dry-run"]);
        assert!(Cli::try_parse_from(args).is_err(), "{number}");
    }
    // 0 clears the track number.
    for number in [0, 2] {
        let mut args = base.to_vec();
        let text = number.to_string();
        args.extend(["--track-number", &text, "--dry-run"]);
        assert!(matches!(
            Cli::try_parse_from(args).unwrap().command,
            Command::Metadata {
                action: super::MetadataCommand::Set {
                    fields: super::metadata_set::Fields {
                        track_number: Some(n),
                        ..
                    },
                    dry_run: true,
                    ..
                }
            } if n == number
        ));
    }
}

#[test]
fn resume_cli_requires_a_snapshot() {
    let base = [
        "tool",
        "resume",
        "--journal",
        "journal",
        "--trace-dir",
        "new-trace",
    ];
    assert!(Cli::try_parse_from(base).is_err());
    let mut args = base.to_vec();
    args.extend(["--snapshot", "snapshot", "--run-dir", "run"]);
    assert!(matches!(
        Cli::try_parse_from(&args).unwrap().command,
        Command::Resume {
            run_dir: Some(_),
            cancel_prepared: None,
            ..
        }
    ));
    args.extend(["--cancel-prepared", "52"]);
    assert!(Cli::try_parse_from(args).is_err());
}

#[test]
fn replacement_flags_parse_and_exclude_each_other() {
    assert!(matches!(
        Cli::try_parse_from([
            "tool",
            "sync",
            "--replace-library",
            "--yes",
            "--batch-size",
            "25"
        ])
        .unwrap()
        .command,
        Command::Sync {
            replace_library: true,
            replace_changed: false,
            yes: true,
            batch_size: 25,
            ..
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["tool", "sync", "--replace-changed", "--dry-run"])
            .unwrap()
            .command,
        Command::Sync {
            replace_changed: true,
            dry_run: true,
            yes: false,
            ..
        }
    ));
    assert!(
        Cli::try_parse_from(["tool", "sync", "--replace-changed", "--replace-library"]).is_err()
    );
}

#[test]
fn replace_library_plan_takes_the_whole_selection_policy() {
    let base = [
        "tool",
        "plan",
        "music",
        "--replace-library",
        "--snapshot",
        "snapshot",
    ];
    assert!(Cli::try_parse_from(base).is_ok());
    for flags in [
        vec!["--add-only"],
        vec!["--include", "part"],
        vec!["--exclude", "part"],
        vec!["--recursive"],
    ] {
        let mut args = base.to_vec();
        args.extend(flags.iter().copied());
        // A replacement plan takes the whole selection policy.
        let accepted = flags[0] != "--add-only";
        assert_eq!(Cli::try_parse_from(args).is_ok(), accepted, "{flags:?}");
    }
}

#[test]
fn catalog_cli_defaults_to_csv_stdout_and_help_is_local() {
    assert!(matches!(
        Cli::try_parse_from(["tool", "catalog"]).unwrap().command,
        Command::Catalog {
            output: None,
            trace_dir: None
        }
    ));
    assert_eq!(
        Cli::try_parse_from(["tool", "catalog", "--help"])
            .err()
            .unwrap()
            .kind(),
        clap::error::ErrorKind::DisplayHelp
    );
    assert!(Cli::try_parse_from(["tool", "catalog", "--ping"]).is_err());
    assert!(matches!(
        Cli::try_parse_from([
            "tool",
            "catalog",
            "--output",
            "new.csv",
            "--trace-dir",
            "new-trace"
        ])
        .unwrap()
        .command,
        Command::Catalog {
            output: Some(_),
            trace_dir: Some(_)
        }
    ));
}

#[test]
fn syslog_filter_keeps_sync_services_without_matching_watch_or_batch() {
    for line in [
        "atc[12]: connected",
        "AirTraffic: startup",
        "ATAirlock: asset",
        "medialibraryd: opened",
        "com.apple.Music: player",
    ] {
        assert!(relevant_syslog(line), "missing relevant line: {line}");
    }
    for line in [
        "watchdog: healthy",
        "AppleWatch: connected",
        "batch job complete",
        "notifications: dispatch",
        "cameracaptured(CMCapture)[73] <Notice>: videoeffects/com-apple-Music/reactions-enabled = 0",
        "SpringBoard[12] <Notice>: com.apple.musicrecognition and com.zhiliaoapp.musically",
    ] {
        assert!(!relevant_syslog(line), "irrelevant line: {line}");
    }
}

#[test]
fn wifi_finds_the_phone_first_only_for_commands_that_reach_it() {
    let reaches = |args: &[&str]| {
        let cli = Cli::try_parse_from([&["rustunes", "--wifi"], args].concat()).unwrap();
        super::reaches_phone(&cli.command)
    };
    assert!(reaches(&["snapshot", "/tmp/snapshot"]));
    assert!(reaches(&["sync", "--dry-run"]));
    assert!(reaches(&["push", "/tmp/music"]));
    assert!(reaches(&["pull"]));
    assert!(!reaches(&["config", "library", "/tmp/music"]));
    assert!(reaches(&["clear"]));
    assert!(reaches(&["device", "info"]));
    assert!(!reaches(&["device", "list"]));
    assert!(!reaches(&["scan", "/tmp/music"]));
    assert!(!reaches(&["inspect", "/tmp/track.m4a"]));
    assert!(!reaches(&["completions", "bash"]));
    let apply = [
        "apply",
        "plan.json",
        "--snapshot",
        "s",
        "--journal",
        "j",
        "--trace-dir",
        "t",
    ];
    assert!(reaches(&apply));
    assert!(!reaches(&[&apply[..], &["--check-only"]].concat()));
}

#[test]
fn sync_mirrors_the_library_without_a_path_or_a_selection() {
    for extra in [&[][..], &["--dry-run", "--yes"], &["--flac-to-alac"]] {
        let args = [&["tool", "sync"][..], extra].concat();
        assert!(Cli::try_parse_from(args).is_ok(), "{extra:?}");
    }
    // Subfolders only when asked for.
    for (args, expected) in [
        (&["tool", "sync"][..], false),
        (&["tool", "sync", "--recursive"], true),
    ] {
        let Command::Sync { recursive, .. } = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected sync");
        };
        assert_eq!(recursive, expected);
    }
    // Sending a path, a file or a selection is `push`; removing is sync's own.
    for extra in [
        &["music"][..],
        &["--add-only"],
        &["--remove-missing"],
        &["--add-remove"],
        &["--include", "a.m4a"],
        &["--exclude", "a.m4a"],
        &["--no-recursive"],
        &["--include-backups"],
        &["--snapshot", "snapshot"],
    ] {
        let args = [&["tool", "sync"][..], extra].concat();
        assert!(Cli::try_parse_from(args).is_err(), "{extra:?}");
    }
}

#[test]
fn push_sends_a_path_or_the_library_and_takes_a_selection() {
    let Command::Push { path, .. } = Cli::try_parse_from(["tool", "push"]).unwrap().command else {
        panic!("expected push");
    };
    assert!(path.is_none());
    let args = [
        "tool",
        "push",
        "music",
        "--include",
        "a.m4a",
        "--exclude",
        "b",
        "--snapshot",
        "s",
        "--dry-run",
        "--batch-size",
        "10",
    ];
    let Command::Push {
        path,
        include,
        batch_size,
        dry_run,
        ..
    } = Cli::try_parse_from(args).unwrap().command
    else {
        panic!("expected push");
    };
    assert_eq!(path.as_deref(), Some(std::path::Path::new("music")));
    assert_eq!((include.len(), batch_size, dry_run), (1, 10, true));
    for (args, expected) in [
        (&["tool", "push"][..], false),
        (&["tool", "push", "--recursive"], true),
    ] {
        let Command::Push { recursive, .. } = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected push");
        };
        assert_eq!(recursive, expected);
    }
    // Push never removes or replaces.
    for extra in [
        &["--yes"][..],
        &["--remove-missing"],
        &["--replace-library"],
        &["--replace-changed"],
    ] {
        let args = [&["tool", "push", "music"][..], extra].concat();
        assert!(Cli::try_parse_from(args).is_err(), "{extra:?}");
    }
}

#[test]
fn pull_takes_tracks_into_a_directory_and_config_sets_the_library() {
    assert!(matches!(
        Cli::try_parse_from(["tool", "pull"]).unwrap().command,
        Command::Pull {
            tracks: false,
            into: None,
            recursive: false,
            ..
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["tool", "pull", "--recursive"])
            .unwrap()
            .command,
        Command::Pull {
            recursive: true,
            ..
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["tool", "pull", "--tracks", "--into", "dir", "--dry-run"])
            .unwrap()
            .command,
        Command::Pull {
            tracks: true,
            into: Some(_),
            dry_run: true,
            ..
        }
    ));
    assert!(Cli::try_parse_from(["tool", "pull", "--into", "dir"]).is_err());
    assert!(matches!(
        Cli::try_parse_from(["tool", "config", "library", "/music"])
            .unwrap()
            .command,
        Command::Config {
            setting: super::ConfigCommand::Library { directory: Some(_) }
        }
    ));
    assert!(matches!(
        Cli::try_parse_from(["tool", "config", "library"])
            .unwrap()
            .command,
        Command::Config {
            setting: super::ConfigCommand::Library { directory: None }
        }
    ));
}

#[test]
fn plan_takes_each_sync_change_that_removes_tracks() {
    let base = ["tool", "plan", "music", "--snapshot", "snapshot"];
    for flags in [
        &["--replace-library"][..],
        &["--replace-changed"],
        &["--remove-missing"],
        &["--remove-missing", "--add-only"],
        &["--add-remove", "--include", "part"],
    ] {
        let mut args = base.to_vec();
        args.extend(flags);
        assert!(Cli::try_parse_from(&args).is_ok(), "{flags:?}");
    }
    for flags in [
        ["--replace-changed", "--replace-library"],
        ["--replace-changed", "--add-only"],
        ["--remove-missing", "--replace-changed"],
        ["--add-remove", "--replace-library"],
    ] {
        let mut args = base.to_vec();
        args.extend(flags);
        assert!(Cli::try_parse_from(&args).is_err(), "{flags:?}");
    }
}

#[test]
fn a_batch_is_at_most_499_operations_so_one_file_holds_it() {
    let commands: [&[&str]; 6] = [
        &["sync"],
        &["push"],
        &[
            "apply",
            "plan.json",
            "--snapshot",
            "s",
            "--journal",
            "j",
            "--trace-dir",
            "t",
        ],
        &["delete", "0000000000000001"],
        &["prune", "music"],
        &["clear"],
    ];
    for command in commands {
        let parse = |size: &str| {
            let mut args = vec!["tool"];
            args.extend_from_slice(command);
            args.extend(["--batch-size", size]);
            Cli::try_parse_from(args)
        };
        for size in ["1", "499"] {
            assert!(parse(size).is_ok(), "{command:?} {size}");
        }
        for size in ["0", "500", "1000"] {
            assert!(parse(size).is_err(), "{command:?} {size}");
        }
    }
}

#[test]
fn sessions_take_499_operations_by_default() {
    let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
    assert!(matches!(
        parse(&["tool", "sync"]),
        Command::Sync {
            batch_size: 499,
            ..
        }
    ));
    assert!(matches!(
        parse(&["tool", "push"]),
        Command::Push {
            batch_size: 499,
            ..
        }
    ));
    assert!(matches!(
        parse(&[
            "tool",
            "apply",
            "plan.json",
            "--snapshot",
            "s",
            "--journal",
            "j",
            "--trace-dir",
            "t",
        ]),
        Command::Apply {
            batch_size: 499,
            ..
        }
    ));
    assert!(matches!(
        parse(&["tool", "sync", "--batch-size", "1"]),
        Command::Sync { batch_size: 1, .. }
    ));
    for removal in [
        &["tool", "delete", "0000000000000001"][..],
        &["tool", "prune", "music"],
        &["tool", "clear"],
    ] {
        match parse(removal) {
            Command::Delete { options, .. }
            | Command::Prune { options, .. }
            | Command::Clear { options } => {
                let request = options.into_request(std::path::PathBuf::new());
                assert_eq!(request.batch_size, 499)
            }
            _ => unreachable!(),
        }
    }
}
