use super::*;

#[test]
fn metadata_plan_cli_requires_complete_request_snapshot_and_journal() {
    let cli = Cli::try_parse_from([
        "rustunes",
        "metadata",
        "plan",
        "00000000000003e9",
        "--request",
        "request.json",
        "--snapshot",
        "snapshot",
        "--journal",
        "journal",
        "--output",
        "intent.json",
    ])
    .unwrap();
    match cli.command {
        Command::Metadata {
            action:
                MetadataCommand::Plan {
                    track,
                    request,
                    snapshot,
                    journal,
                    output,
                },
        } => {
            assert_eq!(track, 1001);
            assert_eq!(request, PathBuf::from("request.json"));
            assert_eq!(snapshot, PathBuf::from("snapshot"));
            assert_eq!(journal, PathBuf::from("journal"));
            assert_eq!(output, Some(PathBuf::from("intent.json")));
        }
        _ => panic!("wrong metadata command"),
    }
    for args in [
        vec![
            "rustunes",
            "metadata",
            "plan",
            "00000000000003e9",
            "--snapshot",
            "snapshot",
            "--journal",
            "journal",
        ],
        vec![
            "rustunes",
            "metadata",
            "plan",
            "00000000000003e9",
            "--request",
            "request.json",
            "--journal",
            "journal",
        ],
        vec![
            "rustunes",
            "metadata",
            "plan",
            "00000000000003e9",
            "--request",
            "request.json",
            "--snapshot",
            "snapshot",
        ],
        vec![
            "rustunes",
            "metadata",
            "plan",
            "0000000000000000",
            "--request",
            "request.json",
            "--snapshot",
            "snapshot",
            "--journal",
            "journal",
        ],
        vec![
            "rustunes",
            "metadata",
            "plan",
            "00000000000003e9",
            "--request",
            "request.json",
            "--snapshot",
            "snapshot",
            "--journal",
            "journal",
            "--execute",
        ],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn metadata_plan_completion_is_derived_without_changing_set() {
    let mut bytes = Vec::new();
    completions::bash(Cli::command(), &mut bytes).unwrap();
    let completion = String::from_utf8(bytes).unwrap();
    assert!(completion.contains("metadata:plan"));
    assert!(completion.contains("metadata/plan"));
    assert!(completion.contains("--request"));
    let set = Cli::try_parse_from([
        "rustunes",
        "metadata",
        "set",
        "00000000000003e9",
        "--track-number",
        "7",
        "--snapshot",
        "snapshot",
        "--journal",
        "journal",
        "--dry-run",
    ])
    .unwrap();
    assert!(matches!(
        set.command,
        Command::Metadata {
            action: MetadataCommand::Set {
                fields: super::metadata_set::Fields {
                    track_number: Some(7),
                    ..
                },
                dry_run: true,
                ..
            }
        }
    ));
}
