use super::*;
use crate::engine::state::held::{self, Drift};
use crate::engine::state::journal::{self, AttemptState};
use music_protocol::catalogue::identity;
use std::{sync::atomic::AtomicU64, time::SystemTime};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "native-sync-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn source(&self) -> PathBuf {
        let path = self.0.join("source.m4a");
        fs::write(
            &path,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        path
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn raw_prefs() -> Vec<u8> {
    let mut raw = vec![0; 1232];
    raw[..4].copy_from_slice(b"frpd");
    raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
    raw[6..8].copy_from_slice(&25_u16.to_le_bytes());
    raw[8] = 1;
    raw[12..20].copy_from_slice(&1_u64.to_le_bytes());
    raw[20..28].copy_from_slice(&2_u64.to_le_bytes());
    raw
}
// Build the ancillary namespace through the reusable catalogue reader. The
// application cannot manufacture the protocol snapshot's private eligibility map.
fn ancillary_snapshot(
    identity: &identity::LibraryIdentity,
    anchor: u32,
) -> music_protocol::protocol::ancillary::EligibleSnapshot {
    let mut sql = String::new();
    for (item, sync, location) in [(2, -888, "ARTW.m4a"), (3, 999, "GENI.m4a")] {
        sql.push_str(&format!(
            "INSERT INTO item SELECT {item},media_type,album_pid,album_artist_pid,
             item_artist_pid,genre_id,composer_pid,base_location_id,remote_location_id,
             track_number,disc_number,is_compilation,composer_order FROM item WHERE item_pid=1;
             INSERT INTO item_extra SELECT {item},title,'{location}',total_time_ms,file_size,
             track_count,disc_count,year,copyright,content_rating,content_rating_level,sort_title,comment
             FROM item_extra WHERE item_pid=1;
             INSERT INTO item_store VALUES({item},{sync});
             INSERT INTO item_state VALUES({item},0,1,1,1);
             INSERT INTO item_stats VALUES({item},0,0,0,0,0,0,0,0);"
        ));
    }
    sql.push_str(
        "INSERT INTO artwork VALUES('100',300,6,0,'aa/00000000000000000000000000000000000000');
         INSERT INTO artwork_token VALUES(2,'100',300,1,0,0);",
    );
    let catalog =
        music_protocol::test_support::catalogue::modified_collection_catalog(anchor, &sql);
    music_protocol::protocol::ancillary::EligibleSnapshot::new(&catalog, identity)
}

fn facts() -> held::Fixture {
    held::Fixture {
        device: "synthetic-device".into(),
        identity: identity::parse(&raw_prefs()).unwrap(),
        anchor: 13,
        database_pid: 100,
        container_pid: 101,
        occupied: BTreeSet::from([100, 101]),
        paths: BTreeSet::new(),
        input_receipt_sha256: [7; 32],
        ancillary: ancillary_snapshot(&identity::parse(&raw_prefs()).unwrap(), 13),
    }
}
fn state() -> HeldState {
    facts().state()
}
fn operation(pid: u64) -> Vec<u8> {
    let dict = |entries: Vec<(&str, Value)>| {
        Value::Dictionary(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    };
    let document = dict(vec![
        ("revision", 14_u64.into()),
        ("timestamp", Value::Date(SystemTime::UNIX_EPOCH.into())),
        (
            "operations",
            Value::Array(vec![
                dict(vec![
                    ("operation", "update_db_info".into()),
                    ("pid", 100_i64.into()),
                    (
                        "db_info",
                        dict(vec![("primary_container_pid", 101_i64.into())]),
                    ),
                ]),
                dict(vec![
                    ("operation", "insert_track".into()),
                    ("pid", (pid as i64).into()),
                    (
                        "item",
                        dict(vec![
                            ("album_pid", 2001_i64.into()),
                            ("artist_pid", 2002_i64.into()),
                        ]),
                    ),
                ]),
            ]),
        ),
    ]);
    let mut bytes = Vec::new();
    document.to_writer_binary(&mut bytes).unwrap();
    bytes
}
fn prepared(source: &Path) -> PreparedAttempt {
    PreparedAttempt {
        kind: AttemptKind::Insert,
        content: Content {
            source: (music_protocol::media::fingerprint::read(source).unwrap()).into(),
            track_pid: 2000,
            path: "iTunes_Control/Music/F39/TEST.m4a".into(),
            artwork: None,
            collection: Some([12; 32]),
        },
        new_album_pid: Some(2001),
        new_artist_pid: Some(2002),
        current_media_anchor: 13,
        operation: operation(2000),
        input_receipt_sha256: [7; 32],
    }
}
fn control() -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(180),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

/// A batch member as the ALAC encoder sends it: checksum 0 without a cover.
fn batch_single(pid: u64) -> Vec<u8> {
    let mut root = Value::from_reader(Cursor::new(operation(pid))).unwrap();
    let ops = root.as_dictionary_mut().unwrap().get_mut("operations");
    let insert = ops.unwrap().as_array_mut().unwrap()[1]
        .as_dictionary_mut()
        .unwrap();
    insert
        .get_mut("item")
        .and_then(Value::as_dictionary_mut)
        .unwrap()
        .insert("artwork_checksum".into(), 0_u64.into());
    let mut bytes = Vec::new();
    root.to_writer_binary(&mut bytes).unwrap();
    bytes
}

#[test]
fn batch_validation_matches_each_insert_to_its_members_cover() {
    use music_protocol::protocol::batch::{combine, with_artwork};
    let temp = Temp::new();
    let path = temp.source();
    let first = prepared(&path).content;
    let mut second = first.clone();
    second.track_pid = 3000;
    second.path = "iTunes_Control/Music/F39/TWOO.m4a".into();
    let art = journal::Artwork {
        cache_id: 140,
        bytes: 1000,
        sha256: [4; 32],
    };
    let mut covered = second.clone();
    covered.artwork = Some(art.clone());
    let check = |members: &[&Content], singles: &[Vec<u8>]| {
        validate_batch(
            &state(),
            &combine(singles).unwrap(),
            members.iter().map(|c| (*c, [None, None])),
        )
    };
    let plain = [batch_single(2000), batch_single(3000)];
    let named = [
        batch_single(2000),
        with_artwork(&batch_single(3000), 140, 1017).unwrap(),
    ];
    check(&[&first, &second], &plain).unwrap();
    check(&[&first, &covered], &named).unwrap();
    // The cover must be named exactly: not missing, not unplanned, not another
    // cache ID or checksum, and only for the members.
    for (members, singles) in [
        ([&first, &covered], plain.to_vec()),
        ([&first, &second], named.to_vec()),
        (
            [&first, &covered],
            vec![
                batch_single(2000),
                with_artwork(&batch_single(3000), 141, 1017).unwrap(),
            ],
        ),
        (
            [&first, &covered],
            vec![
                batch_single(2000),
                with_artwork(&batch_single(3000), 140, 1018).unwrap(),
            ],
        ),
    ] {
        assert!(check(&members, &singles).is_err());
    }
    assert!(check(&[&first], &plain).is_err());
}

#[test]
fn derived_budget_covers_bounded_worst_case_without_expansion() {
    // The largest state a phone `rustunes init` prepared can hold (no CDB)
    // fits one connection beside the largest audio; never expanded past it.
    let largest = facts().largest(false, false).guard_requests();
    assert_eq!(operation_budget(largest, MAX_AUDIO).unwrap(), 7800);
    assert!(operation_budget(largest, MAX_AUDIO + 1).is_err());
    assert!(operation_budget(state().guard_requests(), 936).unwrap() < 1024);
    // With the largest CDB as well, it does not: refused, not expanded.
    let with_cdb = facts().largest(true, false).guard_requests();
    assert!(operation_budget(with_cdb, MAX_AUDIO).is_err());
}

#[test]
fn a_full_operation_files_batch_guard_fits_one_session_connection() {
    let current = facts().largest(true, true);
    let members = crate::engine::planning::change::MAX_BATCH;
    let budget = batch_guard_budget(
        &current,
        members,
        music_protocol::protocol::signing::MAX_OPERATION_BYTES,
    )
    .unwrap();
    assert!(budget > MAX_BOUNDED_OPS, "{budget}");
    // Every member's cover stage, as insert_batch adds them.
    let covers = 2 * (4 * members + 8);
    assert!(budget + covers <= MAX_SESSION_OPS, "{budget}");
}

#[test]
fn fresh_catalog_collision_anchor_receipt_and_database_prefix_guards() {
    let temp = Temp::new();
    let path = temp.source();
    let plan = prepared(&path);
    validate_preparation(&plan, &state()).unwrap();
    for change in 0..6 {
        let mut changed = facts();
        match change {
            0 => {
                changed.occupied.insert(2000);
            }
            1 => {
                changed.occupied.insert(2001);
            }
            2 => {
                changed
                    .paths
                    .insert("iTunes_Control/Music/F39/test.m4a".into());
            }
            3 => changed.anchor += 1,
            4 => changed.input_receipt_sha256 = [8; 32],
            _ => changed.database_pid += 1,
        }
        assert!(validate_preparation(&plan, &changed.state()).is_err());
    }
}

#[test]
fn trace_creation_never_replaces() {
    let temp = Temp::new();
    let trace = temp.0.join("trace");
    create_trace(&trace).unwrap();
    durable_write(&trace, "evidence", b"preserve").unwrap();
    assert!(create_trace(&trace).is_err());
    assert!(durable_write(&trace, "evidence", b"replace").is_err());
    assert_eq!(fs::read(trace.join("evidence")).unwrap(), b"preserve");
}

#[test]
fn deferred_pre_session_drift_creates_no_attempt_or_reservation() {
    let temp = Temp::new();
    let source = temp.source();
    let current = state();
    let identity = Identity::new(current.device_identifier(), current.library_identity()).unwrap();
    let mut journal = Journal::create(&temp.0.join("journal"), &identity).unwrap();
    let ids = journal.reserved_pids().unwrap();
    let paths = journal.reserved_paths().unwrap();
    let mut intent = prepared(&source);
    let input = b"immutable input";
    intent.input_receipt_sha256 = Sha256::digest(input).into();
    let error = finish_read_only_preflight(
        &mut journal,
        &intent,
        &temp.0.join("input.json"),
        input,
        &control(),
        Err(StateReadDrift::fixture("MediaLibrary.sqlitedb-wal", Drift::Size).into()),
    )
    .unwrap_err();
    assert!(error.is::<PreSessionStateDrift>());
    assert!(
        journal.outstanding().unwrap().is_none(),
        "a failed read-only guard must not strand a durable Prepared row"
    );
    assert_eq!(journal.reserved_pids().unwrap(), ids);
    assert_eq!(journal.reserved_paths().unwrap(), paths);
    assert!(!temp.0.join("input.json").exists());
    // The original frozen identities remain usable; no cancellation/reallocation.
    let id = finish_read_only_preflight(
        &mut journal,
        &intent,
        &temp.0.join("input.json"),
        input,
        &control(),
        Ok(()),
    )
    .unwrap();
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
    assert_eq!(fs::read(temp.0.join("input.json")).unwrap(), input);
}

/// A held file that appeared, disappeared or changed during the guard is
/// drift, but grants no refresh: only other bytes in a bound file do.
#[test]
fn deferred_pre_session_drift_during_the_guard_is_not_refresh_authority() {
    for drift in [Drift::Appeared, Drift::Disappeared, Drift::Attributes] {
        let temp = Temp::new();
        let source = temp.source();
        let current = state();
        let identity =
            Identity::new(current.device_identifier(), current.library_identity()).unwrap();
        let mut journal = Journal::create(&temp.0.join("journal"), &identity).unwrap();
        let error = finish_read_only_preflight(
            &mut journal,
            &prepared(&source),
            &temp.0.join("input.json"),
            b"input",
            &control(),
            Err(StateReadDrift::fixture("iTunesCDB", drift).into()),
        )
        .unwrap_err();
        assert!(!error.is::<PreSessionStateDrift>(), "{drift:?}");
        assert!(error.is::<StateReadDrift>(), "{drift:?}");
        assert!(journal.outstanding().unwrap().is_none());
        assert!(!temp.0.join("input.json").exists());
    }
}

#[test]
fn deferred_pre_session_unknown_or_cancelled_failure_is_not_refresh_authority() {
    for cancelled in [false, true] {
        let temp = Temp::new();
        let source = temp.source();
        let current = state();
        let identity =
            Identity::new(current.device_identifier(), current.library_identity()).unwrap();
        let mut journal = Journal::create(&temp.0.join("journal"), &identity).unwrap();
        let control = control();
        control.cancelled.store(cancelled, Ordering::Relaxed);
        let error = finish_read_only_preflight(
            &mut journal,
            &prepared(&source),
            &temp.0.join("input.json"),
            b"input",
            &control,
            Err(anyhow::anyhow!("unknown transport or namespace failure")),
        )
        .unwrap_err();
        assert!(!error.is::<PreSessionStateDrift>());
        assert!(journal.outstanding().unwrap().is_none());
        assert!(journal.reserved_pids().unwrap().is_empty());
    }
}

#[tokio::test]
async fn deferred_source_failure_does_not_prepare_or_poll_connector() {
    let temp = Temp::new();
    let source = temp.source();
    let mut intent = prepared(&source);
    let input = b"new input";
    intent.input_receipt_sha256 = Sha256::digest(input).into();
    let current = held::Fixture {
        input_receipt_sha256: intent.input_receipt_sha256,
        ..facts()
    }
    .state();
    let identity = Identity::new(current.device_identifier(), current.library_identity()).unwrap();
    let mut journal = Journal::create(&temp.0.join("journal"), &identity).unwrap();
    let mut bytes = fs::read(&source).unwrap();
    bytes[20] ^= 1;
    fs::write(&source, bytes).unwrap();
    let called = AtomicBool::new(false);
    let result = execute_new_native_insert(
        &mut journal,
        Insertion {
            attempt: &intent,
            source: &source,
            state: &current,
            host_name: "test",
            trace: &temp.0.join("transfer"),
        },
        &temp.0.join("input.json"),
        input,
        &control(),
        || async {
            called.store(true, Ordering::Relaxed);
            anyhow::bail!("connector must not be polled")
        },
    )
    .await;
    assert!(result.is_err());
    assert!(!called.load(Ordering::Relaxed));
    assert!(journal.outstanding().unwrap().is_none());
    assert!(journal.reserved_pids().unwrap().is_empty());
}

#[test]
fn failed_deferred_guard_preserves_verified_prefix_reservations_and_all_journal_bytes() {
    let temp = Temp::new();
    let source = temp.source();
    let current = state();
    let identity = Identity::new(current.device_identifier(), current.library_identity()).unwrap();
    let journal_dir = temp.0.join("journal");
    let mut journal = Journal::create(&journal_dir, &identity).unwrap();
    let mut prior = prepared(&source);
    prior.content.track_pid = 9000;
    prior.content.path = "iTunes_Control/Music/F39/OLDX.m4a".into();
    prior.new_album_pid = None;
    prior.new_artist_pid = None;
    prior.operation = operation(9000);
    let old_content = prior.content.clone();
    let old = journal.prepare(prior).unwrap();
    let permit = journal
        .mark_started(
            old,
            StartEvidence {
                ready_media_anchor: 13,
                ready_receipt_sha256: [8; 32],
                signature: [1; 21],
            },
        )
        .unwrap();
    journal
        .complete(
            permit,
            journal::CompletionEvidence {
                track_pid: old_content.track_pid,
                path: old_content.path.clone(),
                audio_bytes: old_content.source.file_bytes,
                audio_file_sha256: old_content.source.full_file_sha256,
                artwork_sha256: None,
                media_anchor: 14,
                receipt_sha256: [9; 32],
            },
        )
        .unwrap();
    let before = fs::read(journal_dir.join("journal.sqlite")).unwrap();
    let ids = journal.reserved_pids().unwrap();
    let paths = journal.reserved_paths().unwrap();
    let error = finish_read_only_preflight(
        &mut journal,
        &prepared(&source),
        &temp.0.join("input.json"),
        b"input",
        &control(),
        Err(StateReadDrift::fixture("MediaLibrary.sqlitedb-wal", Drift::Size).into()),
    )
    .unwrap_err();
    assert!(error.is::<PreSessionStateDrift>());
    assert_eq!(
        fs::read(journal_dir.join("journal.sqlite")).unwrap(),
        before
    );
    assert_eq!(journal.attempt(old).unwrap().state, AttemptState::Verified);
    assert!(journal.outstanding().unwrap().is_none());
    assert_eq!(journal.reserved_pids().unwrap(), ids);
    assert_eq!(journal.reserved_paths().unwrap(), paths);
    assert!(
        journal
            .lookup_active_verified(&old_content.source.audio)
            .unwrap()
            .is_some()
    );
}

#[test]
fn two_read_only_drifts_exhaust_without_rows_inputs_or_reservations() {
    let temp = Temp::new();
    let source = temp.source();
    let current = state();
    let identity = Identity::new(current.device_identifier(), current.library_identity()).unwrap();
    let dir = temp.0.join("journal");
    let mut journal = Journal::create(&dir, &identity).unwrap();
    let original = fs::read(dir.join("journal.sqlite")).unwrap();
    let c = control();
    let deadline = c.deadline;
    for _ in 1..=2 {
        let error = finish_read_only_preflight(
            &mut journal,
            &prepared(&source),
            &temp.0.join("input.json"),
            b"input",
            &c,
            Err(StateReadDrift::fixture("MediaLibrary.sqlitedb-wal", Drift::Size).into()),
        )
        .unwrap_err();
        assert!(error.is::<PreSessionStateDrift>());
        assert_eq!(c.deadline, deadline);
        assert!(journal.outstanding().unwrap().is_none());
        assert!(journal.reserved_pids().unwrap().is_empty());
        assert!(journal.reserved_paths().unwrap().is_empty());
    }
    assert_eq!(fs::read(dir.join("journal.sqlite")).unwrap(), original);
    assert!(!temp.0.join("input.json").exists());
}
