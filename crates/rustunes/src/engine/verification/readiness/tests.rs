use super::*;
use music_protocol::catalogue::sqlite::ContainerRow;

fn directory() -> Attributes {
    [("st_ifmt".into(), "S_IFDIR".into())].into_iter().collect()
}
fn file(bytes: u64) -> Attributes {
    [
        ("st_ifmt".into(), "S_IFREG".into()),
        ("st_size".into(), bytes.to_string()),
    ]
    .into_iter()
    .collect()
}

pub(crate) fn fresh_library() -> LibraryState {
    LibraryState {
        user_version: 2_410_000,
        db_info_rows: 0,
        db_info: None,
        media_anchor: None,
        synced_rows: SYNCED.map(|table| (table, 0)),
        items: 0,
        containers: vec![ContainerRow {
            container_pid: 5,
            sync_id: 0,
            name: "Recently Added".into(),
            is_hidden: false,
            distinguished_kind: 0,
            smart: true,
        }],
    }
}
const SYNCED: [&str; 4] = music_protocol::catalogue::sqlite::SYNCED_TABLES;

fn synced_library() -> LibraryState {
    LibraryState {
        db_info_rows: 1,
        db_info: Some((3, 4)),
        media_anchor: Some("1".into()),
        ..fresh_library()
    }
}

/// A phone as Setup Assistant leaves it: a database and nothing else.
pub(crate) fn fresh() -> Inspection {
    let present = [
        "iTunes_Control",
        "iTunes_Control/iTunes",
        device_files::DATABASE,
    ];
    Inspection {
        product_type: "iPhone99,1".into(),
        product_version: "27.2".into(),
        build_version: "24Z999".into(),
        device_name: "iPhone".into(),
        paths: PATHS
            .iter()
            .map(|path| {
                let state = present.contains(path).then(|| {
                    if *path == device_files::DATABASE {
                        file(4096)
                    } else {
                        directory()
                    }
                });
                (*path, state)
            })
            .collect(),
        music: None,
        staged: None,
        prefs: Prefs::Absent,
        temporaries: Vec::new(),
        library: Some(fresh_library()),
    }
}

pub(crate) fn with_prefs(mut inspection: Inspection, record: &InitRecord) -> Inspection {
    let prefs = record.prefs("rustunes").unwrap();
    inspection.prefs = Prefs::Valid {
        identity: prefs.identity().unwrap(),
        raw: prefs.raw,
    };
    for (path, state) in &mut inspection.paths {
        if [device_files::PREFS, device_files::PREFS_PLIST].contains(path) {
            *state = Some(file(1232));
        }
    }
    inspection
}

/// The phone after `init` wrote iTunesPrefs, under `host_name`, and stopped
/// before its plist.
pub(crate) fn with_prefs_only(
    mut inspection: Inspection,
    record: &InitRecord,
    host_name: &str,
) -> Inspection {
    let raw = record.prefs(host_name).unwrap().raw;
    set(&mut inspection, device_files::PREFS);
    inspection.prefs = Prefs::Partial {
        present: device_files::PREFS,
        bytes: raw,
    };
    inspection
}

/// `path` on the phone, holding `bytes`.
pub(crate) fn with_temporary(
    mut inspection: Inspection,
    path: &'static str,
    bytes: Option<Vec<u8>>,
) -> Inspection {
    set(&mut inspection, path);
    inspection.temporaries.push((path, bytes));
    inspection
}

pub(crate) fn set(inspection: &mut Inspection, path: &str) {
    for (name, state) in &mut inspection.paths {
        if *name == path {
            *state = Some(file(696));
        }
    }
}

fn unclear(readiness: Readiness) -> Vec<String> {
    match readiness {
        Readiness::Unclear(reasons) => reasons,
        other => panic!("expected unclear, got {other:?}"),
    }
}

#[test]
fn a_phone_with_only_its_database_or_nothing_is_never_synced() {
    assert_eq!(fresh().readiness(None).unwrap(), Readiness::NeverSynced);
    let mut empty = fresh();
    empty.library = None;
    for (_, state) in &mut empty.paths {
        *state = None;
    }
    assert_eq!(empty.readiness(None).unwrap(), Readiness::NeverSynced);
    let record = InitRecord::mint().unwrap();
    assert_eq!(
        fresh().readiness(Some(&record)).unwrap(),
        Readiness::NeverSynced
    );
}

#[test]
fn rustunes_prefs_on_an_unsynced_library_are_an_unfinished_init() {
    let record = InitRecord::mint().unwrap();
    let mut inspection = with_prefs(fresh(), &record);
    assert_eq!(
        inspection.readiness(Some(&record)).unwrap(),
        Readiness::InitUnfinished
    );
    // Its own staged pair may still be there; nothing else may.
    inspection.staged = Some(vec![
        "Sync_00000000.plist".into(),
        "Sync_00000000.plist.cig".into(),
    ]);
    assert_eq!(
        inspection.readiness(Some(&record)).unwrap(),
        Readiness::InitUnfinished
    );
    inspection.staged = Some(vec!["Sync_00000001.plist.cig".into()]);
    assert_eq!(
        unclear(inspection.readiness(Some(&record)).unwrap()).len(),
        1
    );
}

#[test]
fn another_librarys_prefs_without_a_finished_sync_are_unclear() {
    let apple = InitRecord::mint().unwrap();
    let inspection = with_prefs(fresh(), &apple);
    let reasons = unclear(inspection.readiness(None).unwrap());
    assert!(reasons[0].contains("another computer's library identity"));
    let ours = InitRecord::mint().unwrap();
    assert!(matches!(
        inspection.readiness(Some(&ours)).unwrap(),
        Readiness::Unclear(_)
    ));
}

#[test]
fn a_synced_database_with_prefs_is_synced_with_or_without_apple_files() {
    let record = InitRecord::mint().unwrap();
    let mut inspection = with_prefs(fresh(), &record);
    inspection.library = Some(synced_library());
    assert_eq!(
        inspection.readiness(Some(&record)).unwrap(),
        Readiness::Synced { initialised: true }
    );
    assert_eq!(
        inspection.readiness(None).unwrap(),
        Readiness::Synced { initialised: false }
    );
    set(&mut inspection, device_files::CDB);
    set(&mut inspection, device_files::ARTWORK_DB);
    assert_eq!(
        inspection.readiness(None).unwrap(),
        Readiness::Synced { initialised: false }
    );
}

#[test]
fn signs_of_a_sync_without_prefs_are_unclear() {
    let mut synced = fresh();
    synced.library = Some(synced_library());
    assert!(matches!(
        synced.readiness(None).unwrap(),
        Readiness::Unclear(_)
    ));
    for path in [device_files::CDB, device_files::ARTWORK_DB] {
        let mut inspection = fresh();
        set(&mut inspection, path);
        let reasons = unclear(inspection.readiness(None).unwrap());
        assert!(reasons[0].starts_with(path), "{reasons:?}");
    }
    let mut partial = fresh();
    partial.prefs = Prefs::Partial {
        present: device_files::PREFS_PLIST,
        bytes: b"<plist/>".to_vec(),
    };
    assert!(unclear(partial.readiness(None).unwrap())[0].contains("only"));
    // Without the record that wrote it, iTunesPrefs alone is no one's.
    let record = InitRecord::mint().unwrap();
    let alone = with_prefs_only(fresh(), &record, "rustunes");
    assert!(unclear(alone.readiness(None).unwrap())[0].contains("only"));
    let mut staged = fresh();
    staged.staged = Some(vec!["Sync_00000000.plist.cig".into()]);
    assert!(matches!(
        staged.readiness(None).unwrap(),
        Readiness::Unclear(_)
    ));
    let mut rows = fresh();
    let mut library = fresh_library();
    library.synced_rows[1].1 = 1;
    rows.library = Some(library);
    assert!(matches!(
        rows.readiness(None).unwrap(),
        Readiness::Unclear(_)
    ));
}

#[test]
fn the_report_withholds_the_identity() {
    let record = InitRecord::mint().unwrap();
    let inspection = with_prefs(fresh(), &record);
    let readiness = inspection.readiness(Some(&record)).unwrap();
    let text = inspection.report(&readiness).to_string();
    let library = record.prefs("rustunes").unwrap().identity().unwrap();
    assert!(!text.contains(library.library_id()));
    assert!(text.contains("\"init_unfinished\""));
    assert!(text.contains("\"phone_writes\":0"));
}

#[test]
fn inits_own_prefs_without_their_plist_are_an_unfinished_init() {
    let record = InitRecord::mint().unwrap();
    // Written under another --host-name than a retry may use.
    let inspection = with_prefs_only(fresh(), &record, "Mac mini");
    assert_eq!(
        inspection.readiness(Some(&record)).unwrap(),
        Readiness::InitUnfinished
    );
    let own = inspection.own_prefs(&record).unwrap();
    let written = record.prefs("Mac mini").unwrap();
    assert_eq!(own.raw, written.raw);
    assert_eq!(own.plist, written.plist);

    // Another library's, or with a database that records a sync: unclear.
    let other = InitRecord::mint().unwrap();
    assert!(unclear(inspection.readiness(Some(&other)).unwrap())[0].contains("only"));
    let mut synced = with_prefs_only(fresh(), &record, "rustunes");
    synced.library = Some(synced_library());
    assert!(unclear(synced.readiness(Some(&record)).unwrap())[0].contains("only"));
    // A changed byte is not init's file.
    let mut changed = with_prefs_only(fresh(), &record, "rustunes");
    if let Prefs::Partial { bytes, .. } = &mut changed.prefs {
        bytes[0x30] ^= 1;
    }
    assert!(changed.own_prefs(&record).is_none());
    assert!(matches!(
        changed.readiness(Some(&record)).unwrap(),
        Readiness::Unclear(_)
    ));
}

#[test]
fn inits_own_cut_short_temporaries_are_an_unfinished_init() {
    let record = InitRecord::mint().unwrap();
    let written = record.prefs("Mac mini").unwrap();
    // iTunesPrefs.tmp, before iTunesPrefs: empty, cut short or whole.
    for length in [0, 100, written.raw.len()] {
        let inspection = with_temporary(
            fresh(),
            device_files::PREFS_TEMPORARY,
            Some(written.raw[..length].to_vec()),
        );
        assert_eq!(
            inspection.readiness(Some(&record)).unwrap(),
            Readiness::InitUnfinished,
            "{length}"
        );
        // Cut before the computer name, it fits the image under any name.
        let leftovers = inspection.leftover_temporaries(Some(&record));
        let [(path, Some(writing))] = leftovers.as_slice() else {
            panic!("{length}");
        };
        assert_eq!(*path, device_files::PREFS_TEMPORARY);
        assert!(writing.starts_with(&written.raw[..length]));
        assert_eq!(writing.len(), written.raw.len());
        if length == written.raw.len() {
            assert_eq!(*writing, written.raw);
        }
    }
    // iTunesPrefs.plist.tmp, after iTunesPrefs.
    for length in [0, 100, written.plist.len()] {
        let inspection = with_temporary(
            with_prefs_only(fresh(), &record, "Mac mini"),
            device_files::PREFS_PLIST_TEMPORARY,
            Some(written.plist[..length].to_vec()),
        );
        assert_eq!(
            inspection.readiness(Some(&record)).unwrap(),
            Readiness::InitUnfinished,
            "{length}"
        );
        assert_eq!(
            inspection.leftover_temporaries(Some(&record)),
            [(
                device_files::PREFS_PLIST_TEMPORARY,
                Some(written.plist.clone())
            )]
        );
    }
}

#[test]
fn temporaries_init_did_not_leave_are_unclear() {
    let record = InitRecord::mint().unwrap();
    let written = record.prefs("rustunes").unwrap();
    let other = InitRecord::mint().unwrap().prefs("rustunes").unwrap();
    let mut more = written.raw.clone();
    more.push(0);
    for (inspection, record) in [
        // Another library's identity, being written by Apple Devices.
        (
            with_temporary(
                fresh(),
                device_files::PREFS_TEMPORARY,
                Some(other.raw.clone()),
            ),
            Some(&record),
        ),
        // No record: rustunes init never ran here.
        (
            with_temporary(fresh(), device_files::PREFS_TEMPORARY, Some(Vec::new())),
            None,
        ),
        // Longer than what init writes.
        (
            with_temporary(fresh(), device_files::PREFS_TEMPORARY, Some(more)),
            Some(&record),
        ),
        // Not a file rustunes could read.
        (
            with_temporary(fresh(), device_files::PREFS_TEMPORARY, None),
            Some(&record),
        ),
        // A plist temporary without init's iTunesPrefs before it.
        (
            with_temporary(
                fresh(),
                device_files::PREFS_PLIST_TEMPORARY,
                Some(written.plist[..10].to_vec()),
            ),
            Some(&record),
        ),
    ] {
        let reasons = unclear(inspection.readiness(record).unwrap());
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains(".tmp is on the phone")),
            "{reasons:?}"
        );
    }
}

#[test]
fn the_report_lists_the_prefs_temporaries() {
    let record = InitRecord::mint().unwrap();
    let inspection = with_temporary(
        fresh(),
        device_files::PREFS_TEMPORARY,
        Some(record.prefs("rustunes").unwrap().raw),
    );
    let readiness = inspection.readiness(Some(&record)).unwrap();
    let report = inspection.report(&readiness);
    let paths = report["paths"].as_array().unwrap();
    for path in [
        device_files::PREFS_TEMPORARY,
        device_files::PREFS_PLIST_TEMPORARY,
    ] {
        assert!(paths.iter().any(|entry| entry["path"] == path), "{path}");
    }
    let listed = paths
        .iter()
        .find(|entry| entry["path"] == device_files::PREFS_TEMPORARY)
        .unwrap();
    assert_eq!(listed["present"], true);
    assert_eq!(report["readiness"]["state"], "init_unfinished");
}
