use super::*;
use crate::engine::planning::additive;

fn pins() -> Pinned {
    Pinned {
        removals: [0x3e9, 0x3ea].into(),
        replaced_by: [(0x3e9, "a.m4a".to_owned())].into(),
        additions: [("a.m4a".to_owned(), None), ("new.mp3".to_owned(), None)].into(),
        updates: [(
            "b.m4a".to_owned(),
            json!({"decision":"update","track_id":"00000000000003eb","changes":[]}),
        )]
        .into(),
    }
}

#[test]
fn actions_round_trip_and_say_whether_they_add_only_or_replace() {
    for action in [
        Action::RemoveMissing,
        Action::AddRemove,
        Action::ReplaceChanged,
        Action::ReplaceLibrary,
    ] {
        assert_eq!(Action::parse(action.as_str()).unwrap(), action);
    }
    assert!(Action::parse("additive_dry_run").is_err());
    assert!(!Action::RemoveMissing.add_only());
    assert!(Action::AddRemove.add_only() && !Action::AddRemove.replaces());
    assert!(Action::ReplaceChanged.replaces() && Action::ReplaceLibrary.replaces());
}

#[test]
fn a_removal_must_select_exactly_the_planned_tracks() {
    let pins = pins();
    pins.check_removals(&[0x3e9, 0x3ea].into()).unwrap();
    let error = pins
        .check_removals(&[0x3e9, 0x3ec].into())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only in the plan: 00000000000003ea; only now: 00000000000003ec"),
        "{error}"
    );
    assert!(error.contains("nothing was sent"), "{error}");
    assert!(pins.check_removals(&[0x3e9].into()).is_err());
}

#[test]
fn a_replacement_must_pair_each_track_with_its_planned_source() {
    let pins = pins();
    pins.check_replacements(&[("00000000000003e9".into(), "a.m4a".into())])
        .unwrap();
    let error = pins
        .check_replacements(&[("00000000000003e9".into(), "b.m4a".into())])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only in the plan: 00000000000003e9 → a.m4a"),
        "{error}"
    );
    assert!(pins.check_replacements(&[]).is_err());
}

#[test]
fn differences_list_at_most_five_entries_a_side() {
    let planned: BTreeSet<String> = (0..8).map(|n| format!("p{n}")).collect();
    let fresh = BTreeSet::new();
    assert_eq!(
        differences(&planned, &fresh),
        "only in the plan: p0, p1, p2, p3, p4 and 3 more"
    );
    assert_eq!(differences(&fresh, &fresh), "the same");
}

fn row(path: &str, decision: Decision) -> additive::SourceDecision {
    additive::SourceDecision {
        relative_path: path.into(),
        managed_audio_replacement: None,
        source_file_bytes: None,
        source_file_sha256: None,
        encoded_audio_sha256: None,
        projected: None,
        mp3: None,
        mp3_insertion: None,
        flac_to_alac: false,
        validation_issues: Vec::new(),
        matches: Vec::new(),
        artwork: ArtworkDecision::NoSourceCoverPreserveExisting,
        native_composer: None,
        decision,
    }
}

fn add() -> Decision {
    let group = || additive::Group {
        id: "00000000000004d2".into(),
        wire_id: "1234".into(),
        status: "new",
    };
    Decision::Add {
        track_id: "00000000000003ff".into(),
        wire_id: "1023".into(),
        album: group(),
        artist: group(),
        path: "iTunes_Control/Music/F00/ABCD.m4a".into(),
        pending_collection_ids: Vec::new(),
        fresh_state_required_before_execution: true,
    }
}

fn year(desired: u32) -> Decision {
    Decision::Update {
        track_id: "00000000000003eb".into(),
        changes: vec![additive::FieldDifference {
            field: "year",
            current: json!(2001),
            desired: json!(desired),
        }],
    }
}

fn sync(rows: Vec<additive::SourceDecision>) -> additive::Plan {
    additive::Plan {
        schema: "rustunes.additive-plan",
        mode: "additive_dry_run",
        preserve_existing_tracks: false,
        flac_to_alac: false,
        source_root: "/music".into(),
        selection: additive::SelectionScope {
            base_directory: "/music".into(),
            recursive: true,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
            selected_files: rows.len(),
            selected_bytes: 0,
        },
        exclusions: Vec::new(),
        metadata_comparison_scope: additive::MetadataComparisonScope {
            compared_fields: Vec::new(),
            unverified_fields: Vec::new(),
            ascii_case_equivalent_group_fields: Vec::new(),
            container_byte_drift_reported_separately: true,
        },
        device_sha256: String::new(),
        library_sha256: String::new(),
        database_sha256: String::new(),
        wal_sha256: None,
        media_anchor: 1,
        device_track_count: 1,
        journal_blockers: Vec::new(),
        artwork_import_recent_play_resets: Vec::new(),
        artwork_import_recent_skip_resets: Vec::new(),
        sources: rows,
        preserved_unmatched_device_tracks: Vec::new(),
        phone_deletions: 0,
        execution_authorized_by_plan: false,
    }
}

fn sync_pins() -> Pinned {
    Pinned {
        additions: [
            ("a.m4a".to_owned(), Some("aa".repeat(32))),
            ("new.mp3".to_owned(), None),
        ]
        .into(),
        updates: [(
            "b.m4a".to_owned(),
            serde_json::to_value(year(2002)).unwrap(),
        )]
        .into(),
        ..Pinned::default()
    }
}

#[test]
fn the_sync_after_a_removal_must_add_and_update_exactly_the_planned_sources() {
    let pins = sync_pins();
    let a = |sha: &str| {
        let mut row = row("a.m4a", add());
        row.source_file_sha256 = Some(sha.repeat(32));
        row
    };
    pins.check_sync(&sync(vec![
        a("aa"),
        row("new.mp3", add()),
        row("b.m4a", year(2002)),
        row("kept.m4a", Decision::Keep),
        // Left out without holding the rest.
        row(
            "deep.flac",
            Decision::Skip {
                code: "flac_conversion_refused",
                reason: String::new(),
                message: String::new(),
            },
        ),
    ]))
    .unwrap();
    // The same path with other bytes is another addition.
    let error = pins
        .check_sync(&sync(vec![
            a("bb"),
            row("new.mp3", add()),
            row("b.m4a", year(2002)),
        ]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(
            "only in the plan: a.m4a (file aaaaaaaaaaaa); only now: a.m4a (file bbbbbbbbbbbb)"
        ),
        "{error}"
    );
    let error = pins
        .check_sync(&sync(vec![
            a("aa"),
            row("new.mp3", add()),
            row("extra.m4a", add()),
            row("b.m4a", year(2003)),
        ]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("additions: only now: extra.m4a"), "{error}");
    assert!(
        error.contains("updates: only in the plan: b.m4a"),
        "{error}"
    );
    assert!(error.contains("added and updated nothing"), "{error}");
    let error = pins
        .check_sync(&sync(vec![
            a("aa"),
            row("new.mp3", add()),
            row("b.m4a", year(2002)),
            row(
                "c.m4a",
                Decision::Conflict {
                    code: "multiple_existing_audio_matches",
                    message: String::new(),
                },
            ),
        ]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("now blocked by c.m4a: multiple_existing_audio_matches"),
        "{error}"
    );
    let mut blocked = sync(vec![
        a("aa"),
        row("new.mp3", add()),
        row("b.m4a", year(2002)),
    ]);
    blocked.journal_blockers = vec!["outstanding_prepared_requires_resolution".into()];
    assert!(pins.check_sync(&blocked).is_err());
}

#[test]
fn a_saved_additive_plan_pins_its_additions_and_updates_and_nothing_else() {
    let mut a = row("a.m4a", add());
    a.source_file_sha256 = Some("aa".repeat(32));
    let saved = sync(vec![
        a,
        row("new.mp3", add()),
        row("b.m4a", year(2002)),
        row("kept.m4a", Decision::Keep),
        row(
            "deep.flac",
            Decision::Skip {
                code: "flac_conversion_refused",
                reason: String::new(),
                message: String::new(),
            },
        ),
    ]);
    let expected = sync_pins();
    let pins = Pinned::from_additive(&saved).unwrap();
    assert_eq!(pins, expected);
    // The plan's own rows, planned again from fresh state, pass its pins.
    pins.check_sync(&saved).unwrap();
}

#[test]
fn a_saved_plan_pins_its_removals_replacements_additions_and_updates() {
    let removal = |id: &str, replaced_by: Option<&str>| Removal {
        track_id: id.into(),
        title: "T".into(),
        artist: "A".into(),
        album: "L".into(),
        plays: 3,
        skips: 1,
        file_bytes: Some(10),
        cause: if replaced_by.is_some() {
            "audio_changed"
        } else {
            "source_gone"
        },
        missing_source_path: None,
        replaced_by: replaced_by.map(str::to_owned),
        blockers: Vec::new(),
    };
    let addition = |path: &str| Addition {
        relative_path: path.into(),
        file_bytes: Some(10),
        source_file_sha256: None,
        codec: "alac",
        title: None,
        artist: None,
        album: None,
        has_cover: false,
        cover_left_out: None,
        replaces: None,
    };
    let plan = ChangePlan {
        schema: SCHEMA,
        action: Action::ReplaceChanged.as_str(),
        flac_to_alac: false,
        source_root: "/music".into(),
        selection: Scope {
            recursive: true,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
            selected_files: 2,
        },
        device_sha256: String::new(),
        library_sha256: String::new(),
        database_sha256: String::new(),
        media_anchor: 1,
        device_track_count: 3,
        removals: vec![
            removal("00000000000003e9", Some("a.m4a")),
            removal("00000000000003ea", None),
        ],
        source_path_unknown: Vec::new(),
        additions: vec![addition("a.m4a"), addition("new.mp3")],
        updates: vec![Update {
            relative_path: "b.m4a".into(),
            decision: serde_json::to_value(year(2002)).unwrap(),
        }],
        skipped: Vec::new(),
        pending_covers: Vec::new(),
        untouched_tracks: 0,
        costs: Costs {
            removal_sessions: 1,
            removal_batch_size: DEFAULT_BATCH,
            upload_bytes: 20,
            plays_lost: 6,
            skips_lost: 2,
        },
        order: ["remove", "sync"],
        dependencies: DEPENDENCIES,
        losses: LOSSES,
        blockers: Vec::new(),
        fresh_state_required_before_execution: true,
        execution_authorized_by_plan: false,
    };
    assert!(plan.ready());
    let mut expected = pins();
    expected.updates = sync_pins().updates;
    assert_eq!(plan.pinned().unwrap(), expected);
    // The saved bytes name no identity of a track the sync adds later.
    let saved = serde_json::to_value(&plan).unwrap();
    assert!(saved["additions"][0].get("track_id").is_none());
    assert_eq!(saved["order"], json!(["remove", "sync"]));
}
