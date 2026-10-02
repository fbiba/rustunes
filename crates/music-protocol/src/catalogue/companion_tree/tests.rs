use super::*;
fn attrs(kind: &str, size: u64) -> Attributes {
    BTreeMap::from([
        ("st_ifmt".into(), kind.into()),
        ("st_size".into(), size.to_string()),
        ("st_mtime".into(), "1".into()),
        ("st_atime".into(), "1".into()),
        ("st_nlink".into(), "1".into()),
    ])
}
fn manifest() -> Manifest {
    Manifest {
        schema: "music-protocol.companion-guard-input".into(),
        namespace: Namespace {
            device: "exact-device".into(),
            current_receipt_sha256: "a".repeat(64),
            complete_index_sha256: "b".repeat(64),
            database_sha256: "c".repeat(64),
            wal_sha256: None,
            media_anchor: 1,
        },
        guards: GUARD_PATHS
            .iter()
            .enumerate()
            .map(|(i, p)| GuardSpec {
                path: (*p).into(),
                expected: (i < 4).then(|| Expected {
                    bytes: 2,
                    sha256: if i == 3 {
                        "c".repeat(64)
                    } else {
                        "d".repeat(64)
                    },
                }),
            })
            .collect(),
        canonical_on_the_go_ids: vec![42],
    }
}
fn complete() -> Snapshot {
    let mut s = Snapshot {
        completed: true,
        ancestors: BTreeMap::from([
            ("iTunes_Control".into(), attrs("S_IFDIR", 0)),
            (ROOT.into(), attrs("S_IFDIR", 0)),
        ]),
        names: vec![
            "OnTheGoPlaylist_42.plist".into(),
            "unknown-companion.bin".into(),
        ],
        ..Snapshot::default()
    };
    for n in s.names.clone() {
        insert(&mut s, child(&n).unwrap(), attrs("S_IFREG", 2)).unwrap();
    }
    s.canonical_stats
        .insert(canonical_path(42).unwrap(), Some(attrs("S_IFREG", 2)));
    s
}
#[test]
fn complete_matching_observation_is_descriptive_and_access_time_may_change() {
    let a = complete();
    let mut b = a.clone();
    for e in b.entries.values_mut() {
        e.attributes.insert("st_atime".into(), "99".into());
    }
    for e in b.canonical_stats.values_mut().flatten() {
        e.insert("st_atime".into(), "99".into());
    }
    matching_passes(&a, &b, &manifest()).unwrap();
    let o = outcome(true);
    assert_eq!(o["restoration_or_mutation_authorized"], false);
}
#[test]
fn missing_unlisted_duplicate_and_incomplete_canonical_nodes_refuse() {
    let m = manifest();
    let a = complete();
    let mut bad = a.clone();
    bad.entries.remove(&child("unknown-companion.bin").unwrap());
    assert!(matching_passes(&bad, &bad, &m).is_err());
    let mut bad = a.clone();
    insert(&mut bad, child("unlisted").unwrap(), attrs("S_IFREG", 1)).unwrap();
    assert!(matching_passes(&bad, &bad, &m).is_err());
    let mut bad = a.clone();
    bad.names.push(bad.names[0].clone());
    assert!(matching_passes(&bad, &bad, &m).is_err());
    let mut bad = a.clone();
    bad.canonical_stats.clear();
    assert!(matching_passes(&bad, &bad, &m).is_err());
    let mut bad = a.clone();
    bad.canonical_stats
        .insert(canonical_path(42).unwrap(), None);
    assert!(matching_passes(&bad, &bad, &m).is_err());
    let mut bad = a.clone();
    bad.completed = false;
    assert!(matching_passes(&bad, &bad, &m).is_err());
}
#[test]
fn unsafe_names_unknown_kinds_symlinks_and_changed_attrs_sizes_refuse() {
    for n in [".", "..", "a/b", "a\\b", "bad\0name", ""] {
        assert!(child(n).is_err(), "{n:?}");
    }
    for kind in ["S_IFLNK", "S_IFIFO", "unknown"] {
        let mut s = Snapshot::default();
        assert!(insert(&mut s, child("node").unwrap(), attrs(kind, 1)).is_err());
    }
    let a = complete();
    let mut b = a.clone();
    let p = child("unknown-companion.bin").unwrap();
    b.entries
        .get_mut(&p)
        .unwrap()
        .attributes
        .insert("st_mtime".into(), "2".into());
    assert!(matching_passes(&a, &b, &manifest()).is_err());
    let mut b = a.clone();
    b.entries.get_mut(&p).unwrap().size = 3;
    assert!(matching_passes(&a, &b, &manifest()).is_err());
}
#[test]
fn fixed_guard_paths_required_slots_duplicates_and_aggregate_bounds_refuse() {
    let m = manifest();
    m.validate("exact-device").unwrap();
    let mut bad = m.clone();
    bad.guards[0].expected = None;
    assert!(bad.validate("exact-device").is_err());
    let mut bad = m.clone();
    bad.guards[0].path = "iTunes_Control/Music/F00/x.mp3".into();
    assert!(bad.validate("exact-device").is_err());
    let mut bad = m.clone();
    bad.canonical_on_the_go_ids = vec![42, 42];
    assert!(bad.validate("exact-device").is_err());
    let mut bad = m.clone();
    for g in bad.guards.iter_mut().take(4) {
        g.expected.as_mut().unwrap().bytes = MAX_GUARD_BYTES;
    }
    assert!(bad.validate("exact-device").is_err());
    assert!(m.validate("different-device").is_err());
    assert!(metadata::names(b".\0..\0x\0\0").is_err()); // No guessed double-terminal framing.
    let mut budget = Budget::default();
    budget.names(metadata::MAX_ENTRIES).unwrap();
    assert!(budget.names(1).is_err());
    budget.operations_attempted = metadata::MAX_OPERATIONS;
    assert!(budget.operation().is_err());
}
fn guards(m: &Manifest) -> Vec<Guard> {
    (0..12)
        .map(|i| {
            let spec = &m.guards[i % 6];
            let expected = spec.expected.as_ref();
            Guard {
                path: spec.path.clone(),
                stage: if i < 6 { "before" } else { "after" }.into(),
                expected_bytes: expected.map_or(0, |e| e.bytes),
                expected_sha256: expected.map_or_else(String::new, |e| e.sha256.clone()),
                raw_local_file: format!("/private/guard-{i}.bin"),
                status: if expected.is_some() {
                    "complete"
                } else {
                    "canonically_absent"
                }
                .into(),
                before: expected.map(|e| attrs("S_IFREG", e.bytes)),
                after: expected.map(|e| attrs("S_IFREG", e.bytes)),
                retained_bytes: Some(expected.map_or(0, |e| e.bytes)),
                sha256: expected.map(|e| e.sha256.clone()),
                opened: expected.is_some(),
                close_attempted: expected.is_some(),
                close_acknowledged: expected.is_some(),
                error: None,
            }
        })
        .collect()
}
#[test]
fn guards_require_acknowledged_close_exact_attributes_and_structured_absence() {
    let m = manifest();
    let good = guards(&m);
    matching_guards(&good, &m).unwrap();
    let mut bad = guards(&m);
    bad[0].close_acknowledged = false;
    assert!(matching_guards(&bad, &m).is_err());
    let mut bad = guards(&m);
    bad[6]
        .before
        .as_mut()
        .unwrap()
        .insert("st_mtime".into(), "2".into());
    bad[6].after = bad[6].before.clone();
    assert!(matching_guards(&bad, &m).is_err());
    let mut bad = guards(&m);
    bad[4].status = "complete".into();
    assert!(matching_guards(&bad, &m).is_err());
    let mut bad = guards(&m);
    bad[0].retained_bytes = Some(1);
    assert!(matching_guards(&bad, &m).is_err());
    let mut bad = guards(&m);
    bad.pop();
    assert!(matching_guards(&bad, &m).is_err());
}
#[test]
fn complete_directory_stats_must_join_the_actual_current_guards() {
    let m = manifest();
    let good = guards(&m);
    let mut s = complete();
    for spec in m.guards.iter().take(4) {
        let name = spec.path.rsplit('/').next().unwrap().to_owned();
        s.names.push(name);
        insert(&mut s, spec.path.clone(), attrs("S_IFREG", 2)).unwrap();
    }
    s.names.sort();
    matching_guard_namespace(&s, &good, &m).unwrap();
    let mut changed = s.clone();
    changed
        .entries
        .get_mut(&m.guards[0].path)
        .unwrap()
        .attributes
        .insert("st_mtime".into(), "99".into());
    assert!(matching_guard_namespace(&changed, &good, &m).is_err());
    let mut missing = s.clone();
    let p = m.guards[0].path.clone();
    missing.entries.remove(&p);
    missing.names.retain(|n| child(n).unwrap() != p);
    assert!(matching_guard_namespace(&missing, &good, &m).is_err());
    let mut present = s;
    present.names.push("MediaLibrary.sqlitedb-wal".into());
    present.names.sort();
    insert(&mut present, m.guards[4].path.clone(), attrs("S_IFREG", 0)).unwrap();
    assert!(matching_guard_namespace(&present, &good, &m).is_err());
}
