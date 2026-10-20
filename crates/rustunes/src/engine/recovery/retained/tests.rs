use super::*;
use crate::engine::state::journal::{Content, PreparedAttempt};
use crate::engine::verification::artwork;
use music_protocol::catalogue::artwork_verification;
use music_protocol::media::fingerprint;
use music_protocol::protocol::signing;
use music_protocol::protocol::templates;
use std::{
    os::unix::fs::{DirBuilderExt, symlink},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

mod ordinary_scope_tests;

fn pd<const N: usize>(values: [(&str, Value); N]) -> Dictionary {
    Dictionary::from_iter(values)
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "recovery-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        Self(p)
    }
    fn journal(&self, current: &Current) -> Journal {
        Journal::create(&self.0.join("journal"), &current.identity).unwrap()
    }
    fn prepared(&self) -> PreparedAttempt {
        let source = self.0.join("source.m4a");
        fs::write(
            &source,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/silence-alac.m4a"
            )),
        )
        .unwrap();
        let content = Content {
            source: (fingerprint::read(&source).unwrap()).into(),
            track_pid: 1000,
            path: "iTunes_Control/Music/F39/NEWW.m4a".into(),
            artwork: None,
            collection: None,
        };
        let db = Value::Dictionary(pd([
            ("operation", "update_db_info".into()),
            ("pid", (-10_i64).into()),
            (
                "db_info",
                Value::Dictionary(pd([("primary_container_pid", (-11_i64).into())])),
            ),
        ]));
        let own = Value::Dictionary(pd([
            ("operation", "insert_track".into()),
            ("pid", 1000_i64.into()),
            (
                "item",
                Value::Dictionary(pd([
                    ("album_pid", 2000_i64.into()),
                    ("artist_pid", 3000_i64.into()),
                ])),
            ),
        ]));
        let root = Value::Dictionary(pd([
            ("revision", 14_u64.into()),
            ("timestamp", Value::Date(SystemTime::UNIX_EPOCH.into())),
            ("operations", Value::Array(vec![db, own])),
        ]));
        let mut operation = Vec::new();
        root.to_writer_binary(&mut operation).unwrap();
        PreparedAttempt {
            kind: AttemptKind::Insert,
            content,
            new_album_pid: Some(2000),
            new_artist_pid: Some(3000),
            current_media_anchor: 13,
            operation,
            input_receipt_sha256: [7; 32],
        }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn prefs() -> Vec<u8> {
    let mut p = vec![0; 1232];
    p[..4].copy_from_slice(b"frpd");
    p[4..6].copy_from_slice(&1u16.to_le_bytes());
    p[6..8].copy_from_slice(&25u16.to_le_bytes());
    p[8] = 1;
    p[12..20].copy_from_slice(&1u64.to_le_bytes());
    p[20..28].copy_from_slice(&2u64.to_le_bytes());
    p
}
fn put(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn current() -> Current {
    let catalog = music_protocol::test_support::catalogue::collection_catalog(13);
    let library = music_protocol::catalogue::identity::parse(&prefs()).unwrap();
    let mut cdb = vec![0; 388];
    cdb[..4].copy_from_slice(b"mhbd");
    put(&mut cdb, 4, 244);
    put(&mut cdb, 8, 388);
    put(&mut cdb, 12, 1);
    put(&mut cdb, 16, 117);
    put(&mut cdb, 20, 1);
    cdb[0x48..0x50].copy_from_slice(&catalog.database_pid().to_le_bytes());
    cdb[244..248].copy_from_slice(b"mhsd");
    put(&mut cdb, 248, 96);
    put(&mut cdb, 252, 144);
    put(&mut cdb, 256, 11);
    put(&mut cdb, 340, 1);
    put(&mut cdb, 348, 1);
    Current {
        device: "synthetic".into(),
        identity: Identity::new("synthetic", &library).unwrap(),
        library,
        catalog,
        raw_preferences: prefs(),
        plist_preferences: prefs(),
        cdb,
        artwork_db: None,
        receipt_sha256: [7; 32],
    }
}
fn control() -> Control {
    Control {
        deadline: Instant::now() + Duration::from_secs(180),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
fn start() -> StartEvidence {
    StartEvidence {
        ready_media_anchor: 13,
        ready_receipt_sha256: [8; 32],
        signature: [1; 21],
    }
}

#[test]
fn checked_completion_requires_exact_started_content_and_original_ready() {
    for change_ready in [false, true] {
        let first = Temp::new();
        let current = current();
        let mut original = first.journal(&current);
        let id = original.prepare(first.prepared()).unwrap();
        drop(original.mark_started(id, start()).unwrap());
        let expected = original.outstanding().unwrap().unwrap();
        let ready = original.start_evidence(id).unwrap().unwrap();
        ensure_same_started(&original, &expected, &ready).unwrap();

        let second = Temp::new();
        let mut other = second.journal(&current);
        let mut prepared = second.prepared();
        let mut different_ready = start();
        if change_ready {
            different_ready.signature[20] = 2;
        } else {
            prepared.input_receipt_sha256 = [9; 32];
        }
        let other_id = other.prepare(prepared).unwrap();
        assert_eq!(other_id, id, "IDs alone do not identify the checked intent");
        drop(other.mark_started(other_id, different_ready).unwrap());
        let before = fs::read(second.0.join("journal/journal.sqlite")).unwrap();
        let error = ensure_same_started(&other, &expected, &ready).unwrap_err();
        assert!(format!("{error:#}").contains(if change_ready {
            "Ready evidence differs"
        } else {
            "Started content differs"
        }));
        assert_eq!(
            before,
            fs::read(second.0.join("journal/journal.sqlite")).unwrap()
        );
    }
}

#[test]
fn checked_completion_keeps_original_cancellation_and_deadline_at_transition() {
    // Synthetic local transition-boundary test; this does not qualify physical
    // readbacks, a native operation, or a saved receipt as completion authority.
    for expired in [false, true] {
        let temp = Temp::new();
        let current = current();
        let mut journal = temp.journal(&current);
        let id = journal.prepare(temp.prepared()).unwrap();
        drop(journal.mark_started(id, start()).unwrap());
        let attempt = journal.outstanding().unwrap().unwrap();
        let evidence = CompletionEvidence {
            track_pid: attempt.prepared.content.track_pid,
            path: attempt.prepared.content.path.clone(),
            audio_bytes: attempt.prepared.content.source.file_bytes,
            audio_file_sha256: attempt.prepared.content.source.full_file_sha256,
            artwork_sha256: None,
            media_anchor: 14,
            receipt_sha256: [6; 32],
        };
        let original = control();
        let cancelled = Arc::clone(&original.cancelled);
        let trace = temp.0.join("checked");
        create_trace(&trace).unwrap();
        let checked = CheckedReconciliation {
            report: report("uncertain", Some(&attempt)),
            trace: Some(trace),
            completion: Some(CheckedCompletion {
                attempt: Box::new(attempt),
                start: start(),
                identity: Identity::new(&current.device, &current.library).unwrap(),
                evidence,
                source: Some("/library/source.m4a".into()),
                control: Control {
                    deadline: if expired {
                        Instant::now() - Duration::from_secs(1)
                    } else {
                        original.deadline
                    },
                    cancelled: Arc::clone(&cancelled),
                },
            }),
        };
        if !expired {
            cancelled.store(true, Ordering::Relaxed);
        }
        let before = fs::read(temp.0.join("journal/journal.sqlite")).unwrap();
        let out = checked.finish(&mut journal).unwrap();
        assert_eq!(out.status, "uncertain");
        assert!(out.reason.unwrap().contains(if expired {
            "deadline expired"
        } else {
            "cancelled"
        }));
        assert!(!out.journal_changed);
        assert_eq!(
            before,
            fs::read(temp.0.join("journal/journal.sqlite")).unwrap()
        );
        assert_eq!(
            journal.outstanding().unwrap().unwrap().state,
            AttemptState::ReconciliationRequired
        );
    }
}

#[test]
fn a_checked_completion_records_the_insertions_source_with_its_binding() {
    // Synthetic local transition, as above.
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let id = journal.prepare(temp.prepared()).unwrap();
    drop(journal.mark_started(id, start()).unwrap());
    let attempt = journal.outstanding().unwrap().unwrap();
    let track = attempt.prepared.content.track_pid;
    let evidence = CompletionEvidence {
        track_pid: track,
        path: attempt.prepared.content.path.clone(),
        audio_bytes: attempt.prepared.content.source.file_bytes,
        audio_file_sha256: attempt.prepared.content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 14,
        receipt_sha256: [6; 32],
    };
    let trace = temp.0.join("checked");
    create_trace(&trace).unwrap();
    let checked = CheckedReconciliation {
        report: report("uncertain", Some(&attempt)),
        trace: Some(trace),
        completion: Some(CheckedCompletion {
            attempt: Box::new(attempt),
            start: start(),
            identity: Identity::new(&current.device, &current.library).unwrap(),
            evidence,
            source: Some("/library/source.m4a".into()),
            control: control(),
        }),
    };
    let out = checked.finish(&mut journal).unwrap();
    assert_eq!(out.status, "reconciled_completed", "{:?}", out.reason);
    assert_eq!(
        journal.recorded_sources().unwrap(),
        BTreeMap::from([(track, PathBuf::from("/library/source.m4a"))])
    );
}

#[test]
fn start_evidence_survives_reopen_without_creating_a_write_permit() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let id = journal.prepare(temp.prepared()).unwrap();
    assert!(journal.start_evidence(id).unwrap().is_none());
    let permit = journal.mark_started(id, start()).unwrap();
    drop(permit);
    drop(journal);
    let mut journal = Journal::open(&temp.0.join("journal"), &current.identity).unwrap();
    let proof = journal.start_evidence(id).unwrap().unwrap();
    assert_eq!(proof.ready_media_anchor, 13);
    assert_eq!(proof.ready_receipt_sha256, [8; 32]);
    assert_eq!(proof.signature, [1; 21]);
    assert!(journal.mark_started(id, start()).is_err());
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
}
#[test]
fn cancellation_requires_exact_never_started_id_and_retains_reservations() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let id = journal.prepare(temp.prepared()).unwrap();
    let reserved = (
        journal.reserved_pids().unwrap(),
        journal.reserved_cache_ids().unwrap(),
        journal.reserved_paths().unwrap(),
    );
    let trace = temp.0.join("cancel");
    assert!(cancel_prepared(&mut journal, &current.identity, id.as_i64() + 1, &trace).is_err());
    assert!(!trace.exists());
    let out = cancel_prepared(&mut journal, &current.identity, id.as_i64(), &trace).unwrap();
    assert_eq!(out.status, "cancelled_prepared");
    assert!(out.journal_changed);
    assert!(journal.outstanding().unwrap().is_none());
    assert_eq!(
        reserved,
        (
            journal.reserved_pids().unwrap(),
            journal.reserved_cache_ids().unwrap(),
            journal.reserved_paths().unwrap()
        )
    );
    assert!(
        cancel_prepared(
            &mut journal,
            &current.identity,
            id.as_i64(),
            &temp.0.join("again")
        )
        .is_err()
    );
    assert!(!temp.0.join("again").exists());
    let second = Temp::new();
    let mut journal = second.journal(&current);
    let id = journal.prepare(second.prepared()).unwrap();
    drop(journal.mark_started(id, start()).unwrap());
    assert!(
        cancel_prepared(
            &mut journal,
            &current.identity,
            id.as_i64(),
            &second.0.join("cancel")
        )
        .is_err()
    );
    assert!(!second.0.join("cancel").exists());
}
#[tokio::test]
async fn namespace_prepared_and_missing_terminal_never_discover_or_replay() {
    let temp = Temp::new();
    let mut current = current();
    let mut journal = temp.journal(&current);
    let trace = temp.0.join("trace");
    let connector = || async {
        panic!("offline guard must not discover phone");
        #[allow(unreachable_code)]
        Ok::<DeviceProvider, anyhow::Error>(unreachable!())
    };
    let result = check(
        &journal,
        &current,
        Options {
            current_state_dir: &temp.0.join("state"),
            run_dir: &temp.0,
            trace: &trace,
        },
        &control(),
        connector,
    )
    .await
    .and_then(CheckedReconciliation::into_report)
    .unwrap();
    assert_eq!(result.status, "no_outstanding");
    assert!(!trace.exists());
    let id = journal.prepare(temp.prepared()).unwrap();
    let result = check(
        &journal,
        &current,
        Options {
            current_state_dir: &temp.0.join("state"),
            run_dir: &temp.0,
            trace: &trace,
        },
        &control(),
        connector,
    )
    .await
    .and_then(CheckedReconciliation::into_report)
    .unwrap();
    assert_eq!(result.status, "uncertain");
    assert!(!trace.exists());
    current.device = "wrong-device".into();
    assert!(
        check(
            &journal,
            &current,
            Options {
                current_state_dir: &temp.0.join("state"),
                run_dir: &temp.0,
                trace: &trace
            },
            &control(),
            connector
        )
        .await
        .and_then(CheckedReconciliation::into_report)
        .is_err()
    );
    assert!(!trace.exists());
    current.device = "synthetic".into();
    drop(journal.mark_started(id, start()).unwrap());
    current.catalog = music_protocol::test_support::catalogue::collection_catalog(14);
    let result = check(
        &journal,
        &current,
        Options {
            current_state_dir: &temp.0.join("state"),
            run_dir: &temp.0,
            trace: &trace,
        },
        &control(),
        connector,
    )
    .await
    .and_then(CheckedReconciliation::into_report)
    .unwrap();
    assert_eq!(result.status, "uncertain");
    assert!(result.reason.unwrap().contains("no retained step"));
    assert!(!result.journal_changed);
    assert!(trace.join("recovery.json").is_file());
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
}
#[test]
fn normal_and_outer_run_discovery_selects_digest_not_latest_step() {
    let temp = Temp::new();
    let first = temp.0.join("step-0001");
    let second = temp.0.join("step-0002");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(first.join("input.json"), b"first").unwrap();
    fs::write(second.join("input.json"), b"second").unwrap();
    let (step, before, _) = locate_step(&temp.0, Sha256::digest(b"first").into()).unwrap();
    assert_eq!(step, first);
    assert_eq!(before, temp.0.join("before"));
    let (_, before, _) = locate_step(&temp.0, Sha256::digest(b"second").into()).unwrap();
    assert_eq!(before, first.join("after"));
    let outer = Temp::new();
    fs::write(
        outer.0.join("run.json"),
        br#"{"schema":"rustunes.run","journal":"/not/followed"}"#,
    )
    .unwrap();
    fs::create_dir(outer.0.join("audio")).unwrap();
    fs::rename(&first, outer.0.join("audio/step-0001")).unwrap();
    fs::create_dir(outer.0.join("cover-0001")).unwrap();
    fs::write(outer.0.join("cover-0001/input.json"), b"cover").unwrap();
    assert_eq!(
        locate_step(&outer.0, Sha256::digest(b"first").into())
            .unwrap()
            .0,
        outer.0.join("audio/step-0001")
    );
    assert_eq!(
        locate_step(&outer.0, Sha256::digest(b"cover").into())
            .unwrap()
            .1,
        outer.0.join("cover-0001/before")
    );
    fs::write(outer.0.join("input.json"), b"cover").unwrap();
    assert!(locate_step(&outer.0, Sha256::digest(b"cover").into()).is_err());
}
#[test]
fn evidence_reader_rejects_symlinks_caps_bad_frames_and_ambiguous_children() {
    let temp = Temp::new();
    let path = temp.0.join("regular");
    fs::write(&path, b"bytes").unwrap();
    assert_eq!(read_bounded(&path, 5).unwrap(), b"bytes");
    assert!(read_bounded(&path, 4).is_err());
    symlink(&path, temp.0.join("link")).unwrap();
    assert!(read_bounded(&temp.0.join("link"), 10).is_err());
    assert!(frame(&[0; 3]).is_err());
    assert!(frame(&[1, 0, 0, 0]).is_err());
    symlink(&temp.0, temp.0.join("step-0001")).unwrap();
    assert!(locate_step(&temp.0, [7; 32]).is_err());
}

fn wire(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes).unwrap();
    let mut out = (bytes.len() as u32).to_le_bytes().to_vec();
    out.extend(bytes);
    out
}
fn incoming(command: &str, params: Option<Value>) -> Value {
    let mut d = pd([
        ("Command", command.into()),
        ("Session", 2_u64.into()),
        ("Type", 0_u64.into()),
        ("Id", 1_u64.into()),
    ]);
    if let Some(p) = params {
        d.insert("Params".into(), p);
    }
    Value::Dictionary(d)
}
fn outgoing(command: &str, params: Value) -> Value {
    Value::Dictionary(pd([
        ("Command", command.into()),
        ("Session", 2_u64.into()),
        ("Params", params),
    ]))
}
fn manifest(id: &str, part: u64) -> Value {
    incoming(
        "AssetManifest",
        Some(Value::Dictionary(pd([(
            "AssetManifest",
            Value::Dictionary(pd([(
                "Media",
                Value::Array(vec![Value::Dictionary(pd([
                    ("AssetID", id.into()),
                    ("AssetType", "Music".into()),
                    ("IsDownload", true.into()),
                    (
                        "Variant",
                        Value::Dictionary(pd([("AssetParts", part.into())])),
                    ),
                ]))]),
            )])),
        )]))),
    )
}
fn transcript(
    temp: &Temp,
    prepared: &PreparedAttempt,
    current: &Current,
) -> (PathBuf, StartEvidence, [u8; 32]) {
    let trace = temp.0.join("frames");
    fs::create_dir(&trace).unwrap();
    let vectors: Value = plist::from_bytes(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/grappa-fixed-vectors.plist"
    )))
    .unwrap();
    let reply = vectors.as_array().unwrap()[0].as_dictionary().unwrap()["reply"]
        .as_data()
        .unwrap();
    let mut built = templates::build(&current.library, "test-host", 13, 1).unwrap();
    built
        .finished
        .as_dictionary_mut()
        .unwrap()
        .get_mut("Params")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .get_mut("DataclassAnchors")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("Media".into(), "14".into());
    let ready = incoming(
        "ReadyForSync",
        Some(Value::Dictionary(pd([
            (
                "DataclassAnchors",
                Value::Dictionary(pd([("Media", "13".into()), ("Book", "0".into())])),
            ),
            (
                "DeviceInfo",
                Value::Dictionary(pd([("Grappa", Value::Data(reply.to_vec()))])),
            ),
        ]))),
    );
    let proof = StartEvidence {
        ready_media_anchor: 13,
        ready_receipt_sha256: Sha256::digest(wire(&ready)).into(),
        signature: signing::FixedHostSession::from_reply(&signing::HOST_REQUEST, reply)
            .unwrap()
            .sign(&prepared.operation)
            .unwrap(),
    };
    let own = (prepared.content.track_pid as i64).to_string();
    let kind = prepared.kind;
    let terminal = incoming("SyncFinished", None);
    let hash = Sha256::digest(wire(&terminal)).into();
    let sequence = [
        (
            true,
            Value::Dictionary(pd([("Command", "Ping".into()), ("Session", 1_u64.into())])),
        ),
        (true, built.request),
        (false, ready),
        (true, built.finished),
        (
            false,
            manifest(&own, if kind == AttemptKind::Insert { 1 } else { 2 }),
        ),
        (
            true,
            outgoing(
                "FileComplete",
                Value::Dictionary(pd([
                    ("AssetID", own.into()),
                    ("Dataclass", "Media".into()),
                    (
                        "AssetPath",
                        if kind == AttemptKind::Insert {
                            prepared.content.path.clone()
                        } else {
                            String::new()
                        }
                        .into(),
                    ),
                ])),
            ),
        ),
        (false, terminal),
    ];
    for (n, (out, value)) in sequence.iter().enumerate() {
        fs::write(
            trace.join(format!(
                "{n:03}-{}.atc",
                if *out { "outgoing" } else { "received" }
            )),
            wire(value),
        )
        .unwrap();
    }
    (trace, proof, hash)
}
#[test]
fn retained_signed_completion_verifies_and_tampered_terminal_or_commands_fail_closed() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let prepared = temp.prepared();
    let (trace, start, terminal) = transcript(&temp, &prepared, &current);
    let id = journal.prepare(prepared).unwrap();
    drop(journal.mark_started(id, start).unwrap());
    let attempt = journal.attempt(id).unwrap();
    let start = journal.start_evidence(id).unwrap().unwrap();
    verify_transcript(
        &trace,
        &attempt,
        &start,
        Some(terminal),
        &current.catalog,
        &current,
    )
    .unwrap();
    // Killed between the phone's SyncFinished and the run's receipt: the
    // transcript alone gives the terminal, and still needs one.
    assert_eq!(
        verify_transcript(&trace, &attempt, &start, None, &current.catalog, &current).unwrap(),
        terminal
    );
    let finished = trace.join("006-received.atc");
    let finished_bytes = fs::read(&finished).unwrap();
    fs::remove_file(&finished).unwrap();
    assert!(
        verify_transcript(&trace, &attempt, &start, None, &current.catalog, &current)
            .unwrap_err()
            .to_string()
            .contains("terminal missing")
    );
    fs::write(&finished, finished_bytes).unwrap();
    let complete = trace.join("005-outgoing.atc");
    let original = fs::read(&complete).unwrap();
    let mut wrong = frame(&original).unwrap();
    wrong
        .as_dictionary_mut()
        .unwrap()
        .get_mut("Params")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("AssetID".into(), "999".into());
    fs::write(&complete, wire(&wrong)).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
    fs::write(&complete, &original).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some([9; 32]),
            &current.catalog,
            &current
        )
        .is_err()
    );
    let mut bad_start = journal.start_evidence(id).unwrap().unwrap();
    bad_start.signature[5] ^= 1;
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &bad_start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
    fs::write(
        trace.join("007-outgoing.atc"),
        wire(&outgoing(
            "SyncFailed",
            Value::Dictionary(Dictionary::new()),
        )),
    )
    .unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
    fs::remove_file(trace.join("007-outgoing.atc")).unwrap();
    fs::write(trace.join("007-partial.atc"), [1, 2]).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
    fs::remove_file(trace.join("007-partial.atc")).unwrap();
    fs::remove_file(trace.join("004-received.atc")).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
    assert_eq!(
        journal.outstanding().unwrap().unwrap().state,
        AttemptState::ReconciliationRequired
    );
}
#[test]
fn artwork_retained_completion_requires_own_part_two_and_empty_path() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let mut prepared = temp.prepared();
    prepared.content.track_pid = u64::MAX;
    prepared.content.path = "iTunes_Control/Music/F39/TEST.m4a".into();
    let verified = CompletionEvidence {
        track_pid: prepared.content.track_pid,
        path: prepared.content.path.clone(),
        audio_bytes: prepared.content.source.file_bytes,
        audio_file_sha256: prepared.content.source.full_file_sha256,
        artwork_sha256: None,
        media_anchor: 13,
        receipt_sha256: [6; 32],
    };
    crate::engine::state::journal::test_support::insert_verified_with(
        &mut journal,
        prepared.content.clone(),
        verified,
    );
    prepared.kind = AttemptKind::Artwork;
    prepared.new_album_pid = None;
    prepared.new_artist_pid = None;
    prepared.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 677,
        bytes: 4,
        sha256: Sha256::digest([0xff, 0xd8, 0xff, 0xd9]).into(),
    });
    let cover = Cover {
        checksum: 21,
        sha256: prepared.content.artwork.as_ref().unwrap().sha256,
        bytes: vec![0xff, 0xd8, 0xff, 0xd9],
    };
    prepared.operation = artwork_update::prepare(
        &current.catalog,
        u64::MAX,
        677,
        &cover,
        SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    let (trace, start, terminal) = transcript(&temp, &prepared, &current);
    let id = journal.prepare(prepared).unwrap();
    drop(journal.mark_started(id, start).unwrap());
    assert!(matches!(dispatch(&journal).unwrap(), Dispatch::Started(_)));
    let attempt = journal.attempt(id).unwrap();
    let start = journal.start_evidence(id).unwrap().unwrap();
    verify_transcript(
        &trace,
        &attempt,
        &start,
        Some(terminal),
        &current.catalog,
        &current,
    )
    .unwrap();
    fs::write(trace.join("004-received.atc"), wire(&manifest("-1", 1))).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(terminal),
            &current.catalog,
            &current
        )
        .is_err()
    );
}

#[test]
fn retained_denial_malformed_telemetry_and_scalar_terminal_never_verify() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let prepared = temp.prepared();
    let (trace, start, terminal) = transcript(&temp, &prepared, &current);
    let id = journal.prepare(prepared).unwrap();
    drop(journal.mark_started(id, start).unwrap());
    let attempt = journal.attempt(id).unwrap();
    let start = journal.start_evidence(id).unwrap().unwrap();
    let terminal_path = trace.join("006-received.atc");
    let terminal_bytes = fs::read(&terminal_path).unwrap();
    fs::rename(&terminal_path, trace.join("007-received.atc")).unwrap();
    for value in [
        incoming(
            "SyncAllowed",
            Some(Value::Dictionary(pd([
                ("ManualSync", false.into()),
                ("EducationModeEnabled", false.into()),
                ("AppRemovalAllowed", true.into()),
                ("AutoSync", false.into()),
                ("PurgeAllowed", true.into()),
                ("DataProtected", false.into()),
                ("AppInstallationAllowed", true.into()),
                ("SupportsFavorites", true.into()),
                (
                    "DisabledAssetTypes",
                    Value::Array(
                        ["Photo", "Movie", "TVEpisode", "HomeVideo"]
                            .into_iter()
                            .map(Value::from)
                            .collect(),
                    ),
                ),
            ]))),
        ),
        incoming(
            "Progress",
            Some(Value::Dictionary(pd([(
                "OverallProgress",
                Value::Real(f64::NAN),
            )]))),
        ),
        incoming("AssetMetrics", Some("invalid".into())),
        incoming("InstalledAssets", None),
    ] {
        fs::write(&terminal_path, wire(&value)).unwrap();
        assert!(
            verify_transcript(
                &trace,
                &attempt,
                &start,
                Some(terminal),
                &current.catalog,
                &current
            )
            .is_err()
        );
        assert_eq!(
            journal.outstanding().unwrap().unwrap().state,
            AttemptState::ReconciliationRequired
        );
    }
    fs::remove_file(trace.join("007-received.atc")).unwrap();
    let bad_terminal = wire(&incoming("SyncFinished", Some("scalar".into())));
    fs::write(&terminal_path, &bad_terminal).unwrap();
    assert!(
        verify_transcript(
            &trace,
            &attempt,
            &start,
            Some(Sha256::digest(&bad_terminal).into()),
            &current.catalog,
            &current
        )
        .is_err()
    );
    fs::write(&terminal_path, terminal_bytes).unwrap();
    verify_transcript(
        &trace,
        &attempt,
        &start,
        Some(terminal),
        &current.catalog,
        &current,
    )
    .unwrap();
}

#[test]
fn artwork_recovery_input_selects_its_album_policy_and_binds_protected_album_proof() {
    let temp = Temp::new();
    let current = current();
    let mut journal = temp.journal(&current);
    let id = journal.prepare(temp.prepared()).unwrap();
    let mut attempt = journal.attempt(id).unwrap();
    let before = music_protocol::test_support::catalogue::artwork_catalog(677, 678);
    let target = &before.tracks()[0];
    attempt.prepared.kind = AttemptKind::Artwork;
    attempt.prepared.content.track_pid = target.sync_id;
    attempt.prepared.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 679,
        bytes: 200,
        sha256: [3; 32],
    });
    let other = json!({"schema":"rustunes.insertion-input"});
    assert!(validate_input_schema(&other, AttemptKind::Artwork).is_err());
    let mut input = json!({"schema":"rustunes.artwork-input",
        "artwork_verification":"exact_track_source_and_decoded_derivative",
        "album_policy":"native_representative_new_or_prior_cache","preserved_album_original":null,
        "current_state_receipt_sha256":hex(&[7;32])});
    assert!(validate_input_schema(&input, AttemptKind::Artwork).is_ok());
    assert!(
        artwork_policy(&input, &attempt, &before).unwrap()
            == Some(ArtworkPolicy::IndependentRepresentative)
    );
    assert!(validate_input_schema(&input, AttemptKind::Insert).is_err());
    let cached = &before.artwork().cached_objects()[&(678, 0)];
    input["album_policy"] = json!("preserve_existing_original_bytes");
    input["preserved_album_original"] = json!({
        "schema":"rustunes.permanent-album-original",
        "album_sync_id":format!("{:016x}",target.album_sync_id),"cache_id":678,
        "media_anchor":before.persisted_media_anchor(),"state_receipt_sha256":hex(&[7;32]),
        "database_sha256":hex(&before.source_hashes().database),
        "wal_sha256":before.source_hashes().wal.map(|h|hex(&h)),
        "source_cover_sha256":hex(&[3;32]),"original_exact_source_bytes":true,"phone_writes":0,
        "original":{"bytes":200,"sha256":hex(&[3;32]),
            "afc_path":format!("{}/Originals/{}",artwork::ARTWORK_ROOT,cached.relative_path)}});
    assert!(
        artwork_policy(&input, &attempt, &before).unwrap()
            == Some(ArtworkPolicy::PreserveAlbum(target.album_sync_id))
    );
    for (key, value) in [
        ("cache_id", json!(999)),
        ("state_receipt_sha256", json!(hex(&[8; 32]))),
        ("original_exact_source_bytes", json!(false)),
        ("album_sync_id", json!("0000000000000001")),
    ] {
        let mut bad = input.clone();
        bad["preserved_album_original"][key] = value;
        assert!(artwork_policy(&bad, &attempt, &before).is_err(), "{key}");
    }
    input["album_policy"] = json!("native_representative_new_or_prior_cache");
    assert!(artwork_policy(&input, &attempt, &before).is_err());
    input["album_policy"] = json!("unknown");
    assert!(artwork_policy(&input, &attempt, &before).is_err());
    input["schema"] = json!("rustunes.artwork-input-unknown");
    assert!(validate_input_schema(&input, AttemptKind::Artwork).is_err());
}

#[test]
fn strict_current_artwork_success_does_not_require_optional_after_artifacts() {
    let temp = Temp::new();
    let mut fresh = current();
    let mut journal = temp.journal(&fresh);
    let id = journal.prepare(temp.prepared()).unwrap();
    let mut attempt = journal.attempt(id).unwrap();
    let cover = Cover {
        checksum: 21,
        sha256: Sha256::digest([0xff, 0xd8, 0xff, 0xd9]).into(),
        bytes: vec![0xff, 0xd8, 0xff, 0xd9],
    };
    attempt.prepared.kind = AttemptKind::Artwork;
    attempt.prepared.content.track_pid = u64::MAX;
    attempt.prepared.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 677,
        bytes: 4,
        sha256: cover.sha256,
    });
    attempt.prepared.operation = artwork_update::prepare(
        &fresh.catalog,
        u64::MAX,
        677,
        &cover,
        SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    let step = temp.0.join("step");
    fs::DirBuilder::new().mode(0o700).create(&step).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(step.join("after"))
        .unwrap();
    fs::write(
        step.join("after/state-receipt.json"),
        b"malformed optional evidence",
    )
    .unwrap();
    let retained = RetainedRun {
        before_database: PathBuf::new(),
        before_wal: None,
        step,
        before: music_protocol::test_support::catalogue::collection_catalog(13),
        cover: Some(cover),
        ready_sha256: [1; 32],
        terminal_sha256: [2; 32],
        pending_sha256: Some([3; 32]),
        artwork_policy: Some(ArtworkPolicy::IndependentRepresentative),
        source: None,
    };
    fresh.catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        14,
        "INSERT INTO artwork VALUES('677',300,6,0,'aa/00000000000000000000000000000000000000');
         INSERT INTO artwork_token VALUES(1,'677',300,1,0,0);
         INSERT INTO artwork_token VALUES(31,'677',300,6,4,0);",
    );
    artwork_verification::verify_independent_representative(
        &retained.before,
        &fresh.catalog,
        u64::MAX,
        677,
    )
    .unwrap();
    let output = temp.0.join("unused-output");
    let proof = verify_artwork_catalog(
        &attempt,
        &retained,
        &fresh,
        &Options {
            run_dir: &temp.0,
            trace: &output,
            current_state_dir: &temp.0.join("missing-optional-current-copy"),
        },
        control().deadline,
    )
    .expect("already-strict fresh artwork transition must not load optional after artifacts");
    assert_eq!(proof["media_anchor"], 14);
    assert!(!output.exists());
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
}

#[test]
fn artwork_without_an_immediate_after_absorbs_listening_from_the_fresh_capture() {
    let temp = Temp::new();
    let mut fresh = current();
    let mut journal = temp.journal(&fresh);
    let id = journal.prepare(temp.prepared()).unwrap();
    let mut attempt = journal.attempt(id).unwrap();
    let cover = Cover {
        checksum: 21,
        sha256: Sha256::digest([0xff, 0xd8, 0xff, 0xd9]).into(),
        bytes: vec![0xff, 0xd8, 0xff, 0xd9],
    };
    attempt.prepared.kind = AttemptKind::Artwork;
    attempt.prepared.content.track_pid = u64::MAX;
    attempt.prepared.content.artwork = Some(crate::engine::state::journal::Artwork {
        cache_id: 677,
        bytes: 4,
        sha256: cover.sha256,
    });
    attempt.prepared.operation = artwork_update::prepare(
        &fresh.catalog,
        u64::MAX,
        677,
        &cover,
        SystemTime::UNIX_EPOCH,
    )
    .unwrap();
    // The run stopped before its After capture: there is no step/after.
    let step = temp.0.join("step");
    fs::DirBuilder::new().mode(0o700).create(&step).unwrap();
    let dates = "ALTER TABLE item_stats ADD COLUMN date_played INTEGER NOT NULL DEFAULT 0;";
    let retained = RetainedRun {
        before_database: PathBuf::new(),
        before_wal: None,
        step,
        before: music_protocol::test_support::catalogue::modified_collection_catalog(13, dates),
        cover: Some(cover),
        ready_sha256: [1; 32],
        terminal_sha256: [2; 32],
        pending_sha256: None,
        artwork_policy: Some(ArtworkPolicy::IndependentRepresentative),
        source: None,
    };
    let art =
        "INSERT INTO artwork VALUES('677',300,6,0,'aa/00000000000000000000000000000000000000');
         INSERT INTO artwork_token VALUES(1,'677',300,1,0,0);
         INSERT INTO artwork_token VALUES(31,'677',300,6,4,0);";
    let check = |fresh: &Current, retained: &RetainedRun, output: &str| {
        verify_artwork_catalog(
            &attempt,
            retained,
            fresh,
            &Options {
                run_dir: &temp.0,
                trace: &temp.0.join(output),
                current_state_dir: &temp.0,
            },
            control().deadline,
        )
    };
    // Two plays since the write.
    fresh.catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
        14,
        &format!(
            "{dates}{art}UPDATE item_stats SET play_count_user=2,play_count_recent=2,has_been_played=1,date_played=800000000;"
        ),
    );
    let proof = check(&fresh, &retained, "listened").unwrap();
    assert_eq!(proof["schema"], "rustunes.artwork-recovery-later-run");
    let listened = &proof["original_artwork_transition"]["tracks_listened_between_runs"];
    assert_eq!(listened[0]["plays"], 2);
    // A date without its play, or a rating, is not listening.
    for change in [
        "UPDATE item_stats SET date_played=800000000;",
        "UPDATE item_stats SET play_count_user=1,play_count_recent=1,has_been_played=1,date_played=800000000,user_rating=60;",
    ] {
        fresh.catalog = music_protocol::test_support::catalogue::modified_collection_catalog(
            14,
            &format!("{dates}{art}{change}"),
        );
        let error = check(&fresh, &retained, "refused").unwrap_err().to_string();
        assert!(
            error.contains("later-run projection also refused"),
            "{error}"
        );
    }
    assert_eq!(journal.attempt(id).unwrap().state, AttemptState::Prepared);
}

#[test]
fn refreshed_insertion_selects_hash_bound_step_local_before_without_touching_prefix() {
    let temp = Temp::new();
    let run = temp.0.join("run");
    fs::create_dir(&run).unwrap();
    let first = run.join("step-0001");
    fs::create_dir(&first).unwrap();
    fs::create_dir(first.join("after")).unwrap();
    fs::write(first.join("after/verified-prefix"), b"keep original prefix").unwrap();
    let step = run.join("step-0002");
    fs::create_dir(&step).unwrap();
    for n in 1..=2 {
        let phase = step.join(format!("preflight-{n:04}"));
        fs::create_dir(&phase).unwrap();
        fs::create_dir(phase.join("before")).unwrap();
        fs::create_dir(phase.join("transfer")).unwrap();
        fs::write(phase.join("before/state-receipt.json"), [n as u8]).unwrap();
    }
    let input = serde_json::to_vec(&json!({"schema":"rustunes.insertion-input",
        "pre_session_attempt":2,
        "current_state_receipt_sha256":hex(&Sha256::digest([2u8]))}))
    .unwrap();
    fs::write(step.join("input.json"), &input).unwrap();
    fs::write(step.join("preflight-0002/input.json"), &input).unwrap();
    let (found, before, bytes) = locate_step(&run, Sha256::digest(&input).into()).unwrap();
    assert_eq!(found, step);
    assert_eq!(bytes, input);
    assert_eq!(
        before,
        step.join("preflight-0002/before"),
        "recovery must not infer previous verified after as a refreshed operation's before"
    );
    assert_eq!(
        fs::read(first.join("after/verified-prefix")).unwrap(),
        b"keep original prefix"
    );
}

#[test]
fn ordinary_insertion_refuses_unknown_attempts_and_input_substitution() {
    let good = json!({"schema":"rustunes.insertion-input","pre_session_attempt":1});
    assert!(check_pre_session(&good, AttemptKind::Insert).is_ok());
    assert!(check_pre_session(&good, AttemptKind::Artwork).is_err());
    assert!(check_pre_session(&json!({}), AttemptKind::Insert).is_err());
    assert!(check_pre_session(&json!({}), AttemptKind::Artwork).is_ok());
    for value in [json!(0), json!(3), json!("1"), Json::Null] {
        let mut changed = good.clone();
        changed["pre_session_attempt"] = value;
        assert!(check_pre_session(&changed, AttemptKind::Insert).is_err());
    }
    let temp = Temp::new();
    let step = temp.0.join("step-0001");
    fs::create_dir(&step).unwrap();
    let phase = step.join("preflight-0001");
    fs::create_dir(&phase).unwrap();
    let bytes = serde_json::to_vec(&good).unwrap();
    fs::write(step.join("input.json"), &bytes).unwrap();
    fs::write(phase.join("input.json"), b"substituted input").unwrap();
    assert!(locate_step(&temp.0, Sha256::digest(&bytes).into()).is_err());
    fs::remove_file(phase.join("input.json")).unwrap();
    fs::remove_dir(&phase).unwrap();
    std::os::unix::fs::symlink(&temp.0, &phase).unwrap();
    assert!(locate_step(&temp.0, Sha256::digest(&bytes).into()).is_err());
}

#[test]
fn ordinary_full_sql_covers_only_an_insertion_creating_its_groups() {
    let temp = Temp::new();
    let prepared = temp.prepared();
    let source = metadata::read(&temp.0.join("source.m4a")).unwrap();
    assert!(ordinary_sql::covers(&prepared, &source).unwrap());
    // Joining an album or artist already on the phone goes by the catalogue
    // verifier instead.
    let mut reused = temp.prepared();
    reused.new_album_pid = None;
    assert!(!ordinary_sql::covers(&reused, &source).unwrap());
    let mut reused = temp.prepared();
    reused.new_artist_pid = None;
    assert!(!ordinary_sql::covers(&reused, &source).unwrap());
}

#[test]
fn a_recovered_insertion_records_the_source_its_retained_plan_named() {
    let temp = Temp::new();
    let plan = br#"{"source_root":"/library","sources":[]}"#;
    let input = |digest: [u8; 32]| json!({"plan_sha256":hex(&digest),"source":"Album/One.m4a"});
    let bound = input(Sha256::digest(plan).into());
    // Without its plan, or with another plan, nothing is recorded.
    assert_eq!(planned_source(&temp.0, &bound).unwrap(), None);
    fs::write(temp.0.join("plan.json"), plan).unwrap();
    assert_eq!(
        planned_source(&temp.0, &bound).unwrap(),
        Some(PathBuf::from("/library/Album/One.m4a"))
    );
    assert_eq!(planned_source(&temp.0, &input([1; 32])).unwrap(), None);
}

#[test]
fn mpeg_started_dispatch_refuses_before_capture_or_replay() {
    use music_protocol::media::fingerprint::{EncodedMpeg1Layer3, TransferredSource};
    let temp = Temp::new();
    let current = current();
    let directory = temp.0.join("journal");
    drop(temp.journal(&current));
    let mut journal = Journal::open(&directory, &current.identity).unwrap();
    let mut prepared = temp.prepared();
    prepared.content.source = TransferredSource::mpeg(
        844,
        [23; 32],
        EncodedMpeg1Layer3 {
            sample_rate: 44100,
            channels: 2,
            frame_count: 2,
            audio_offset: 10,
            audio_bytes: 834,
            trailing_bytes: 0,
            frame_bytes_sha256: [24; 32],
        },
    )
    .unwrap();
    prepared.content.path = "iTunes_Control/Music/F39/NEWW.mp3".into();
    let id = crate::engine::state::journal::test_support::mpeg_started_storage_fixture(
        &mut journal,
        &prepared,
        &start(),
    );
    drop(journal);
    let journal = Journal::open_read_only(&directory, &current.identity).unwrap();
    assert_eq!(
        journal.attempt(id).unwrap().state,
        AttemptState::ReconciliationRequired
    );
    let before = fs::read(directory.join("journal.sqlite")).unwrap();
    match dispatch(&journal).unwrap() {
        Dispatch::Local(result) => {
            assert_eq!(result.status, "uncertain");
            assert!(!result.journal_changed && !result.replay_attempted);
            assert!(
                result
                    .reason
                    .unwrap()
                    .contains("MPEG-1 Layer III insertion recovery is unqualified")
            );
        }
        _ => panic!("MPEG must refuse before any capture callback"),
    }
    assert_eq!(before, fs::read(directory.join("journal.sqlite")).unwrap());
}
