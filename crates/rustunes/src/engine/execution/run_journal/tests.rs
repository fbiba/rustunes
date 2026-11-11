//! Which journal a run uses: explicit, current, absent, carried forward or
//! started for a phone `init` prepared, on a dry run only in its run
//! directory and never from a saved snapshot; and which one it may write.
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::*;
use crate::engine::state::{init_record, journal::test_support};

/// A capture saved as a run saves it, under `directory`, of a phone
/// `record` initialised (`device` its digest) that holds no track.
fn initialised_capture(
    directory: &Path,
    device: &[u8; 32],
    record: &init_record::InitRecord,
) -> Snapshot {
    let hex = |bytes: &[u8]| music_protocol::protocol::transfer::hex(bytes);
    for path in [directory.to_path_buf(), directory.join("catalog")] {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let database = directory.join("catalog/MediaLibrary.sqlitedb");
    music_protocol::test_support::catalogue::copy_closed_collection_fixture(&database);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(&format!(
            "UPDATE db_info SET db_pid={};
             DELETE FROM item; DELETE FROM item_extra; DELETE FROM item_store;
             DELETE FROM item_state; DELETE FROM item_stats;",
            record.database_pid as i64
        ))
        .unwrap();
    connection.close().unwrap();
    let catalog =
        music_protocol::catalogue::sqlite::read_snapshot(&database, None, &std::env::temp_dir())
            .unwrap();
    assert!(catalog.tracks().is_empty());
    let prefs = record.prefs("rustunes").unwrap().raw;
    let library = music_protocol::catalogue::identity::parse(&prefs).unwrap();
    let hashes = catalog.source_hashes();
    let index = serde_json::json!({
        "schema":"rustunes.device-audio-index","complete":true,
        "identity":{"device_identifier_sha256":hex(device),
            "library_identity_sha256":hex(&library.binding_sha256())},
        "catalog":{"database_sha256":hex(&hashes.database),"database_bytes":hashes.database_bytes,
            "wal_sha256":null,"wal_bytes":null,"persisted_media_anchor":catalog.persisted_media_anchor()},
        "track_count":0,"total_bytes":0,"alac_track_count":0,"other_track_count":0,
        "records":[],"other_records":[],"unfilled_records":[],
        "encoded_audio_kind":"encoded-alac-not-pcm","progress_sha256":hex(&Sha256::digest(b"progress")),
        "completed_unix_time_ns":"1","fresh_prefs_before_after_components_equal":true,
        "atomic_snapshot_claim":false,"catalog_metadata":{},
    });
    for name in [
        "before-iTunesPrefs",
        "before-iTunesPrefs.plist",
        "after-iTunesPrefs",
        "after-iTunesPrefs.plist",
    ] {
        fs::write(directory.join(name), &prefs).unwrap();
    }
    fs::write(
        directory.join("complete-index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    fs::write(directory.join("progress.jsonl"), b"progress").unwrap();
    for file in fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .chain([database])
    {
        if file.is_file() {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    snapshot::open(directory).unwrap()
}

/// A capture of a phone holding no track that `init` prepared from another
/// state directory, and the state directory `t/state` beside it, which holds
/// nothing for that phone yet.
fn elsewhere_capture(t: &test_support::Temp) -> (Snapshot, PathBuf) {
    let device = [7; 32];
    // Another state directory's record: the capture's library and database.
    let record = init_record::load_or_create(&t.0.join("elsewhere"), &device).unwrap();
    let saved = initialised_capture(&t.0.join("capture"), &device, &record);
    (saved, t.0.join("state"))
}

/// Where the journal for the phone and library `saved` shows belongs in
/// `state`.
fn phone_journal(state: &Path, saved: &Snapshot) -> PathBuf {
    layout::journal_path(
        state,
        &saved.identity.device_identifier_sha256,
        &saved.identity.library_identity_sha256,
    )
    .unwrap()
}

/// A state directory whose journal for the phone `saved` shows was written by
/// another version (`Superseded`) and binds a track the capture does not
/// show: the earlier journal a run carries forward. `init` did not prepare
/// the phone from it. Returns the state directory and the phone's journal.
fn superseded_state(t: &test_support::Temp, saved: &Snapshot) -> (PathBuf, PathBuf) {
    let state = t.0.join("state");
    let journal = phone_journal(&state, saved);
    let mut j = Journal::create(&journal, &saved.journal_identity().unwrap()).unwrap();
    test_support::insert_verified(&mut j, test_support::alac_content(1));
    drop(j);
    // Another version's schema.
    rusqlite::Connection::open(journal.join("journal.sqlite"))
        .unwrap()
        .execute_batch("CREATE TABLE later_addition(x)")
        .unwrap();
    assert!(superseded_now(&journal, saved));
    (state, journal)
}

fn superseded_now(journal: &Path, saved: &Snapshot) -> bool {
    match Journal::open_read_only(journal, &saved.journal_identity().unwrap()) {
        Ok(_) => false,
        Err(error) => journal::is_superseded(&error),
    }
}

/// The names in `directory`, sorted.
fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every path under `directory`, relative to it, sorted.
fn tree(directory: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, directory: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(directory, directory, &mut out);
    out.sort();
    out
}

/// A new run directory numbered `n` in `state`.
fn new_run(state: &Path, n: u32) -> PathBuf {
    let run = state.join(format!("runs/{n}-99999999"));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&run)
        .unwrap();
    run
}

/// A capture of a phone holding no track, the state directory beside it
/// with that phone's superseded journal, and a new run directory in it.
fn carry_fixture(t: &test_support::Temp) -> (Snapshot, PathBuf, PathBuf, PathBuf) {
    let (saved, _) = elsewhere_capture(t);
    let (state, journal) = superseded_state(t, &saved);
    let run = new_run(&state, 1);
    (saved, state, journal, run)
}

fn control() -> Control {
    Control {
        deadline: Instant::now() + std::time::Duration::from_secs(30),
        cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

/// The journal a run in `run` resolves for `saved`, taken as `taken`.
async fn resolve(
    state: &Path,
    run: &Path,
    saved: &Snapshot,
    taken: Taken,
    dry_run: bool,
    explicit: Option<&Path>,
) -> Result<RunJournal> {
    RunJournal::resolve(
        Ask {
            state,
            run,
            explicit,
            dry_run,
            library: None,
            recursive: false,
        },
        saved,
        taken,
        &control(),
    )
    .await
}

/// A phone `rustunes init` prepared from this state directory, holding no
/// track a sync made, gets the journal a pull would write without one; any
/// other phone does not.
#[test]
fn only_a_phone_init_prepared_that_holds_no_track_starts_its_journal_unasked() {
    use music_protocol::test_support::catalogue::modified_collection_catalog;
    let t = test_support::Temp::new();
    let state = t.0.join("state");
    let device = [7; 32];
    let record = init_record::load_or_create(&state, &device).unwrap();
    let prefs = record.prefs("rustunes").unwrap().raw;
    let initialised = |database: u64, tracks: bool| {
        let mut sql = format!("UPDATE db_info SET db_pid={};", database as i64);
        if !tracks {
            sql.push_str(
                "DELETE FROM item; DELETE FROM item_extra; DELETE FROM item_store;
                 DELETE FROM item_state; DELETE FROM item_stats;",
            );
        }
        modified_collection_catalog(1, &sql)
    };
    let empty = initialised(record.database_pid, false);
    assert!(empty.tracks().is_empty());
    let check = |state: &Path,
                 device: &[u8; 32],
                 prefs: &[u8],
                 catalog: &music_protocol::catalogue::sqlite::Catalog,
                 index_empty: bool| {
        initialised_without_tracks(state, device, prefs, catalog, index_empty).unwrap()
    };
    assert!(check(&state, &device, &prefs, &empty, true));
    // A track in the catalogue or the audio index.
    assert!(!check(
        &state,
        &device,
        &prefs,
        &initialised(record.database_pid, true),
        true
    ));
    assert!(!check(&state, &device, &prefs, &empty, false));
    // Another phone, another library, another database, another state.
    assert!(!check(&state, &[8; 32], &prefs, &empty, true));
    let other = init_record::InitRecord::mint().unwrap();
    let other_prefs = other.prefs("rustunes").unwrap().raw;
    assert!(!check(&state, &device, &other_prefs, &empty, true));
    assert!(!check(
        &state,
        &device,
        &prefs,
        &initialised(record.database_pid ^ 1, false),
        true
    ));
    assert!(!check(
        &t.0.join("elsewhere"),
        &device,
        &prefs,
        &empty,
        true
    ));

    // What it writes is what a pull of that phone writes: no binding.
    let saved = initialised_capture(&t.0.join("capture"), &device, &record);
    let journal = t.path();
    pull::write_unbound(&journal, &saved).unwrap();
    let opened = Journal::open_read_only(&journal, &saved.journal_identity().unwrap()).unwrap();
    assert!(opened.active_bindings().unwrap().is_empty());
    assert!(opened.recorded_sources().unwrap().is_empty());
}

/// The commands that carry the journal forward start the journal of a phone
/// init prepared that holds no track: never from a saved snapshot, which
/// may be older than the tracks the phone holds now, and on a dry run only
/// in the run directory. Once started, the phone's journal is current.
#[tokio::test]
async fn the_journal_of_a_phone_init_prepared_is_started_only_from_a_capture_of_this_run() {
    let t = test_support::Temp::new();
    let state = t.0.join("state");
    let device = [7; 32];
    let record = init_record::load_or_create(&state, &device).unwrap();
    let saved = initialised_capture(&t.0.join("capture"), &device, &record);
    let phone_journal = phone_journal(&state, &saved);
    let start = |n: u32, taken: Taken, dry_run: bool| {
        let (state, saved) = (&state, &saved);
        async move {
            let run = new_run(state, n);
            let journal = resolve(state, &run, saved, taken, dry_run, None)
                .await
                .unwrap();
            (journal, run)
        }
    };

    // A saved snapshot leaves the phone without a journal, as before.
    for (n, dry_run) in [(1, true), (2, false)] {
        let (journal, _) = start(n, Taken::Saved, dry_run).await;
        assert_eq!(journal.path(), phone_journal);
        assert_eq!(journal.resolution(), Resolution::Absent);
        assert!(!journal.exists().unwrap());
        assert!(!phone_journal.try_exists().unwrap());
    }
    // A dry run plans against a journal in its run directory.
    let (journal, run) = start(3, Taken::ByThisRun, true).await;
    assert_eq!(journal.path(), run.join("journal"));
    assert_eq!(
        journal.resolution(),
        Resolution::Started { installed: false }
    );
    assert!(!phone_journal.try_exists().unwrap());
    let identity = saved.journal_identity().unwrap();
    assert!(
        Journal::open_read_only(journal.path(), &identity)
            .unwrap()
            .active_bindings()
            .unwrap()
            .is_empty()
    );
    // The real run starts the phone's journal, which binds nothing.
    let (journal, _) = start(4, Taken::ByThisRun, false).await;
    assert_eq!(journal.path(), phone_journal);
    assert_eq!(
        journal.resolution(),
        Resolution::Started { installed: true }
    );
    let opened = Journal::open_read_only(&phone_journal, &identity).unwrap();
    assert!(opened.active_bindings().unwrap().is_empty());
    assert!(opened.recorded_sources().unwrap().is_empty());
    // The next run uses it as it is.
    let (journal, run) = start(5, Taken::ByThisRun, false).await;
    assert_eq!(journal.path(), phone_journal);
    assert_eq!(journal.resolution(), Resolution::Current);
    assert!(names(&run).is_empty(), "{:?}", names(&run));
}

/// A phone journal this build opens is the one a run uses, from a saved
/// snapshot too: nothing is read beside it or written.
#[tokio::test]
async fn the_phone_journal_this_build_opens_is_current() {
    let t = test_support::Temp::new();
    let (saved, state) = elsewhere_capture(&t);
    let journal = phone_journal(&state, &saved);
    drop(Journal::create(&journal, &saved.journal_identity().unwrap()).unwrap());
    for (n, taken, dry_run) in [
        (1, Taken::ByThisRun, false),
        (2, Taken::ByThisRun, true),
        (3, Taken::Saved, false),
    ] {
        let run = new_run(&state, n);
        let before = tree(&state);
        let resolved = resolve(&state, &run, &saved, taken, dry_run, None)
            .await
            .unwrap();
        assert_eq!(resolved.resolution(), Resolution::Current);
        assert_eq!(resolved.path(), journal);
        assert!(resolved.exists().unwrap());
        assert_eq!(tree(&state), before);
    }
}

/// `--journal` names the journal a run uses, as it is: nothing in the state
/// directory is read or written, not even a superseded journal of the
/// phone, and no earlier journal is copied into the run directory.
#[tokio::test]
async fn an_explicit_journal_is_used_as_it_is() {
    let t = test_support::Temp::new();
    let (saved, state, phone_journal, run) = carry_fixture(&t);
    let before = tree(&state);
    let explicit = t.0.join("mine/journal");
    for (taken, dry_run) in [(Taken::ByThisRun, false), (Taken::Saved, true)] {
        let resolved = resolve(&state, &run, &saved, taken, dry_run, Some(&explicit))
            .await
            .unwrap();
        assert_eq!(resolved.resolution(), Resolution::Explicit);
        assert_eq!(resolved.path(), explicit);
        assert!(!resolved.exists().unwrap());
    }
    assert_eq!(tree(&state), before);
    assert!(names(&run).is_empty(), "{:?}", names(&run));
    assert!(superseded_now(&phone_journal, &saved));
    // A relative path is taken from the working directory.
    let relative = resolve(
        &state,
        &run,
        &saved,
        Taken::ByThisRun,
        false,
        Some(Path::new("journal")),
    )
    .await
    .unwrap();
    assert_eq!(
        relative.path(),
        std::env::current_dir().unwrap().join("journal")
    );
}

/// A dry run carries a superseded journal forward as `pull --dry-run` does:
/// into its run directory, where it plans against it, leaving the phone's
/// journal as it was (README: `delete --dry-run` saves its list "without
/// changing the phone or the journal").
#[tokio::test]
async fn a_dry_run_carries_the_journal_forward_only_into_its_run_directory() {
    let t = test_support::Temp::new();
    let (saved, state, phone_journal, run) = carry_fixture(&t);
    let directory = phone_journal.parent().unwrap();
    let before = names(directory);
    let journal = resolve(&state, &run, &saved, Taken::ByThisRun, true, None)
        .await
        .unwrap();
    assert!(
        names(directory) == before
            && superseded_now(&phone_journal, &saved)
            && journal.path().starts_with(&run),
        "a dry run changed the state directory: {before:?} became {:?}; the phone's journal \
         superseded: {}; planned against {}; the run directory holds {:?}",
        names(directory),
        superseded_now(&phone_journal, &saved),
        journal.path().display(),
        names(&run),
    );
    assert_eq!(
        journal.resolution(),
        Resolution::Carried { installed: false }
    );
    // It plans against the carried journal: the capture shows no track.
    let carried =
        Journal::open_read_only(journal.path(), &saved.journal_identity().unwrap()).unwrap();
    assert!(carried.active_bindings().unwrap().is_empty());
}

/// A real run installs the journal it carries forward as the phone's, and
/// the superseded one stays beside it as `journal.previous`.
#[tokio::test]
async fn a_real_run_installs_the_journal_it_carries_forward() {
    let t = test_support::Temp::new();
    let (saved, state, phone_journal, run) = carry_fixture(&t);
    let journal = resolve(&state, &run, &saved, Taken::ByThisRun, false, None)
        .await
        .unwrap();
    assert_eq!(journal.path(), phone_journal);
    assert_eq!(
        journal.resolution(),
        Resolution::Carried { installed: true }
    );
    assert!(!superseded_now(&phone_journal, &saved));
    assert!(superseded_now(
        &phone_journal.with_file_name("journal.previous"),
        &saved
    ));
}

/// A saved snapshot (`--snapshot`) may be older than the tracks the phone
/// holds now, so it carries no journal forward, as it starts none for a
/// phone `init` prepared: the command refuses, naming `pull`, and the phone's
/// journal stays as it was, on a dry run and on a real run alike.
#[tokio::test]
async fn a_saved_snapshot_never_carries_the_journal_forward() {
    for dry_run in [true, false] {
        let t = test_support::Temp::new();
        let (saved, state, phone_journal, run) = carry_fixture(&t);
        let directory = phone_journal.parent().unwrap();
        let before = names(directory);
        let result = resolve(&state, &run, &saved, Taken::Saved, dry_run, None).await;
        assert!(
            names(directory) == before && superseded_now(&phone_journal, &saved),
            "a saved snapshot (dry run: {dry_run}) changed the state directory: {before:?} \
             became {:?}; the phone's journal superseded: {}; returned {:?}",
            names(directory),
            superseded_now(&phone_journal, &saved),
            result.as_ref().map_err(|e| format!("{e:#}")),
        );
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("rustunes pull"), "{error}");
    }
}

/// Only the journal a run may write is opened for writing: one a dry run
/// carried or started in its run directory refuses, an absent one is
/// created, and an existing one is opened as it is.
#[tokio::test]
async fn only_a_journal_a_run_may_write_is_opened_for_writing() {
    // Carried and started on a dry run.
    let t = test_support::Temp::new();
    let (saved, state, _, run) = carry_fixture(&t);
    let identity = saved.journal_identity().unwrap();
    let carried = resolve(&state, &run, &saved, Taken::ByThisRun, true, None)
        .await
        .unwrap();
    let t = test_support::Temp::new();
    let state = t.0.join("state");
    let record = init_record::load_or_create(&state, &[7; 32]).unwrap();
    let initialised = initialised_capture(&t.0.join("capture"), &[7; 32], &record);
    let started = resolve(
        &state,
        &new_run(&state, 1),
        &initialised,
        Taken::ByThisRun,
        true,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        started.resolution(),
        Resolution::Started { installed: false }
    );
    for in_run in [carried, started] {
        let before = tree(in_run.path());
        let error = format!("{:#}", in_run.writable(&identity).err().unwrap());
        assert!(error.contains("dry run"), "{error}");
        assert_eq!(tree(in_run.path()), before);
    }

    // Absent: created where it belongs, then opened as it is.
    let t = test_support::Temp::new();
    let (saved, state) = elsewhere_capture(&t);
    let identity = saved.journal_identity().unwrap();
    let absent = resolve(
        &state,
        &new_run(&state, 1),
        &saved,
        Taken::ByThisRun,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(absent.resolution(), Resolution::Absent);
    assert!(!absent.exists().unwrap());
    let mut created = absent.writable(&identity).unwrap();
    test_support::insert_verified(&mut created, test_support::alac_content(3));
    drop(created);
    assert!(absent.exists().unwrap());
    let current = resolve(
        &state,
        &new_run(&state, 2),
        &saved,
        Taken::ByThisRun,
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(current.resolution(), Resolution::Current);
    let opened = current.writable(&identity).unwrap();
    assert_eq!(opened.active_bindings().unwrap().len(), 1);
}

/// A saved snapshot is opened where it is, canonical, and taken as saved,
/// with the identity of its phone's journal.
#[test]
fn a_saved_snapshot_is_taken_as_saved() {
    let t = test_support::Temp::new();
    let (saved, _) = elsewhere_capture(&t);
    let captured = Captured::saved(&t.0.join("capture/../capture"), Absent::NONE).unwrap();
    assert_eq!(captured.taken, Taken::Saved);
    assert_eq!(
        captured.path,
        fs::canonicalize(t.0.join("capture")).unwrap()
    );
    let identity = saved.journal_identity().unwrap();
    assert_eq!(
        (
            captured.identity.device_digest(),
            captured.identity.library_digest()
        ),
        (identity.device_digest(), identity.library_digest())
    );
    assert_eq!(
        captured.snapshot.index_receipt_sha256,
        saved.index_receipt_sha256
    );
    let error = format!(
        "{:#}",
        Captured::saved(&t.0.join("nothing"), Absent::NONE)
            .err()
            .unwrap()
    );
    assert!(error.contains("opening supplied snapshot"), "{error}");
}

/// What runs reported about their journal: started or carried, where, and
/// whether it was installed as the phone's.
type Reported = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, PathBuf, bool)>>>;

/// An engine whose observer records what runs report about their journal.
fn observed() -> (crate::engine::Engine, Reported) {
    let reported = Reported::default();
    let sink = std::sync::Arc::clone(&reported);
    let engine = crate::engine::Engine::new(crate::engine::test_engine().config, move |event| {
        let entry = match event {
            crate::engine::Event::JournalStarted { journal, installed } => {
                ("started", journal.to_path_buf(), installed)
            }
            crate::engine::Event::JournalCarried {
                journal, installed, ..
            } => ("carried", journal.to_path_buf(), installed),
            _ => return Ok(()),
        };
        sink.lock().unwrap().push(entry);
        Ok(())
    });
    (engine, reported)
}

/// Starting a journal is reported as carrying one is, with where it went
/// and whether it was installed as the phone's (not on a dry run). A
/// journal used as it is, or left absent, is not reported.
#[tokio::test]
async fn starting_or_carrying_a_journal_is_reported_with_whether_it_was_installed() {
    let (engine, reported) = observed();
    let take = |reported: &Reported| std::mem::take(&mut *reported.lock().unwrap());

    let t = test_support::Temp::new();
    let state = t.0.join("state");
    let record = init_record::load_or_create(&state, &[7; 32]).unwrap();
    let saved = initialised_capture(&t.0.join("capture"), &[7; 32], &record);
    let phone = phone_journal(&state, &saved);
    let run = new_run(&state, 1);
    engine
        .scope(resolve(&state, &run, &saved, Taken::Saved, true, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), []);
    let run = new_run(&state, 2);
    engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, true, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), [("started", run.join("journal"), false)]);
    let run = new_run(&state, 3);
    engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, false, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), [("started", phone.clone(), true)]);
    let run = new_run(&state, 4);
    engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, false, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), []);

    let t = test_support::Temp::new();
    let (saved, state, phone, run) = carry_fixture(&t);
    engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, true, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), [("carried", run.join("journal"), false)]);
    let run = new_run(&state, 2);
    engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, false, None))
        .await
        .unwrap();
    assert_eq!(take(&reported), [("carried", phone, true)]);
}

/// A mirroring `sync` plans its additions against the journal its removal
/// used. On a dry run that is the journal the removal carried forward into
/// its run directory, named as `--journal` names one: the sync uses it as it
/// is and carries nothing again, so the phone's covers are read once and the
/// phone's journal stays as it was.
#[tokio::test]
async fn a_journal_a_dry_run_carried_is_used_again_without_a_second_carry() {
    let (engine, reported) = observed();
    let t = test_support::Temp::new();
    let (saved, state, phone_journal, run) = carry_fixture(&t);
    let removal = engine
        .scope(resolve(&state, &run, &saved, Taken::ByThisRun, true, None))
        .await
        .unwrap();
    assert_eq!(
        removal.resolution(),
        Resolution::Carried { installed: false }
    );
    reported.lock().unwrap().clear();
    let carried = tree(removal.path());
    let sync = new_run(&state, 2);
    let resolved = engine
        .scope(resolve(
            &state,
            &sync,
            &saved,
            Taken::ByThisRun,
            true,
            Some(removal.path()),
        ))
        .await
        .unwrap();
    assert_eq!(resolved.resolution(), Resolution::Explicit);
    assert_eq!(resolved.path(), removal.path());
    assert_eq!(*reported.lock().unwrap(), []);
    assert!(names(&sync).is_empty(), "{:?}", names(&sync));
    assert_eq!(tree(removal.path()), carried);
    assert!(superseded_now(&phone_journal, &saved));
}
