//! A phone's first session: anchors "0" against a phone with no Media anchor,
//! then only SyncFinished or telemetry may follow the metadata.
use super::*;
use crate::catalogue::prefs;
use crate::protocol::templates;

/// The preparation a first session uses: the minted identity and anchor 0.
fn first_prepared() -> (Prepared, String) {
    let identity = prefs::adoption(0x1122, 0x3344, "rustunes")
        .unwrap()
        .identity()
        .unwrap();
    let built = templates::build(
        &identity,
        "rustunes",
        0,
        templates::QUALIFIED_VOICE_MEMO_ANCHOR,
    )
    .unwrap();
    let library = identity.library_id().to_owned();
    (
        prepare(built.request, &built.finished, &library, 0).unwrap(),
        library,
    )
}

fn ready_at_zero() -> Value {
    let mut ready = ready_message();
    set(
        &mut ready,
        &["Params", "DataclassAnchors", "Media"],
        "0".into(),
    );
    ready
}

/// Run a first session against `messages` after the metadata; return what
/// the wait gave and what the host sent.
async fn transcript(messages: Vec<Value>) -> (bool, Vec<Value>) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let (prepared, _) = first_prepared();
    let host = async {
        let now = Instant::now();
        let mut session = Session::begin(
            client,
            prepared,
            &trace.0,
            now + Duration::from_secs(2),
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        session.finish_metadata().await.unwrap();
        let first = session.wait_first_session().await.is_ok();
        // The wait is consumed either way.
        assert!(session.wait_first_session().await.is_err());
        assert!(session.finish_metadata().await.is_err());
        first
    };
    let device = async {
        let mut sent = vec![outgoing(&mut peer).await.0];
        startup(&mut peer).await;
        sent.push(outgoing(&mut peer).await.0);
        peer.write_all(&wire(&ready_at_zero())).await.unwrap();
        sent.push(outgoing(&mut peer).await.0);
        for message in &messages {
            peer.write_all(&wire(message)).await.unwrap();
        }
        sent
    };
    tokio::join!(host, device)
}

#[tokio::test]
async fn a_first_session_requests_at_zero_and_finishes_at_one() {
    let progress = incoming(
        "Progress",
        Some(dict(&[("OverallProgress", Value::Real(0.5))])),
    );
    let (finished, sent) = transcript(vec![
        progress,
        allowed(),
        incoming("SyncFinished", Some(dict(&[]))),
    ])
    .await;
    assert!(finished);
    let (_, library) = first_prepared();
    let command = |value: &Value| value.as_dictionary().unwrap()["Command"].clone();
    assert_eq!(command(&sent[0]).as_string(), Some("Ping"));
    let request = sent[1].as_dictionary().unwrap();
    assert_eq!(command(&sent[1]).as_string(), Some("RequestingSync"));
    let params = request["Params"].as_dictionary().unwrap();
    let anchors = params["DataclassAnchors"].as_dictionary().unwrap();
    assert_eq!(anchors["Media"].as_string(), Some("0"));
    assert_eq!(anchors["VoiceMemo"].as_string(), Some("1"));
    let host = params["HostInfo"].as_dictionary().unwrap();
    assert_eq!(host["LibraryID"].as_string(), Some(library.as_str()));
    let end = sent[2].as_dictionary().unwrap();
    assert_eq!(
        command(&sent[2]).as_string(),
        Some("FinishedSyncingMetadata")
    );
    let params = end["Params"].as_dictionary().unwrap();
    let anchors = params["DataclassAnchors"].as_dictionary().unwrap();
    assert_eq!(anchors["Media"].as_string(), Some("1"));
    assert_eq!(anchors["VoiceMemo"].as_string(), Some("1"));
    let types = params["SyncTypes"].as_dictionary().unwrap();
    assert_eq!(types["Media"].as_unsigned_integer(), Some(1));
}

#[tokio::test]
async fn any_request_or_failure_ends_a_first_session() {
    for late in [
        incoming("AssetManifest", Some(manifest("999", 1))),
        incoming("RequestingSync", Some(dict(&[]))),
        incoming("SyncFailed", Some(dict(&[]))),
    ] {
        let (finished, _) = transcript(vec![late]).await;
        assert!(!finished);
    }
}

mod whole {
    use super::*;
    use crate::catalogue::mutation;
    use crate::protocol::transfer::{
        CaptureControl, MAX_BOUNDED_OPS, METADATA, SIGNATURE, Transfer,
    };
    use crate::test_support::afc::{FakePhone, Node};
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    const PREFS: &str = "iTunes_Control/iTunes/iTunesPrefs";
    const PREFS_PLIST: &str = "iTunes_Control/iTunes/iTunesPrefs.plist";

    struct Ran {
        result: Result<()>,
        phone: FakePhone,
        persisted: Option<(Vec<u8>, [u8; 21])>,
        finished_sent: bool,
    }

    /// Run `finish_first_session` against a phone holding `phone`'s tree,
    /// whose ATC side answers FinishedSyncingMetadata with SyncFinished.
    async fn run(phone: FakePhone, write_prefs: bool, stop: bool) -> Ran {
        let prefs = adopted();
        run_with(phone, &[], write_prefs.then_some(&prefs), stop).await
    }

    /// The pair these tests write.
    fn adopted() -> prefs::Prefs {
        prefs::adoption(0x1122, 0x3344, "rustunes").unwrap()
    }

    async fn run_with(
        phone: FakePhone,
        leftovers: &[(&str, &[u8])],
        prefs: Option<&prefs::Prefs>,
        stop: bool,
    ) -> Ran {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let control = CaptureControl {
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (endpoint, afc_peer) = tokio::io::duplex(64 * 1024);
        let mut afc =
            Transfer::from_test_duplex(endpoint, &trace.0, &control, MAX_BOUNDED_OPS).unwrap();
        let server =
            tokio::spawn(phone.serve(Idevice::new(Box::new(afc_peer), "offline-afc-peer")));
        let (prepared, _) = first_prepared();
        let persisted = Mutex::new(None);
        let host = async {
            let now = Instant::now();
            let mut session = Session::begin(
                client,
                prepared,
                &trace.0,
                now + Duration::from_secs(3),
                now + Duration::from_secs(1),
            )
            .await
            .unwrap();
            let operation =
                mutation::first_session(5, 6, "iPhone", std::time::SystemTime::now()).unwrap();
            let result = session
                .finish_first_session(
                    &mut afc,
                    operation,
                    leftovers,
                    prefs,
                    |operation, signature| {
                        *persisted.lock().unwrap() = Some((operation.to_vec(), *signature));
                        Ok(())
                    },
                    || {
                        if stop {
                            anyhow::bail!("cancelled")
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            drop(afc);
            result
        };
        let device = async {
            outgoing(&mut peer).await;
            startup(&mut peer).await;
            outgoing(&mut peer).await;
            peer.write_all(&wire(&ready_at_zero())).await.unwrap();
            let mut length = [0; 4];
            // The next frame, if any, is FinishedSyncingMetadata.
            // A closed connection is no frame: the host sent nothing more.
            if matches!(
                tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut length)).await,
                Ok(Ok(_))
            ) {
                let mut payload = vec![0; u32::from_le_bytes(length) as usize];
                peer.read_exact(&mut payload).await.unwrap();
                let value: Value = plist::from_bytes(&payload).unwrap();
                assert_eq!(
                    value.as_dictionary().unwrap()["Command"].as_string(),
                    Some("FinishedSyncingMetadata")
                );
                peer.write_all(&wire(&incoming("SyncFinished", Some(dict(&[])))))
                    .await
                    .unwrap();
                true
            } else {
                false
            }
        };
        let (result, finished_sent) = tokio::join!(host, device);
        Ran {
            result,
            phone: server.await.unwrap(),
            persisted: persisted.into_inner().unwrap(),
            finished_sent,
        }
    }

    fn file(phone: &FakePhone, path: &str) -> Vec<u8> {
        match phone.tree.get(path) {
            Some(Node::File(bytes)) => bytes.clone(),
            other => panic!("{path}: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fresh_phone_gets_prefs_folders_and_the_staged_pair_before_finish() {
        let ran = run(
            FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]),
            true,
            false,
        )
        .await;
        ran.result.unwrap();
        assert!(ran.finished_sent);
        let phone = ran.phone;
        let prefs = prefs::adoption(0x1122, 0x3344, "rustunes").unwrap();
        assert_eq!(file(&phone, PREFS), prefs.raw);
        assert_eq!(file(&phone, PREFS_PLIST), prefs.plist);
        assert!(!phone.tree.keys().any(|path| path.ends_with(".tmp")));
        assert_eq!(
            phone.tree.get("iTunes_Control/Sync"),
            Some(&Node::Directory)
        );
        assert_eq!(
            phone.tree.get("iTunes_Control/Sync/Media"),
            Some(&Node::Directory)
        );
        let (operation, signature) = ran.persisted.unwrap();
        assert_eq!(file(&phone, METADATA), operation);
        assert_eq!(file(&phone, SIGNATURE), signature);
        assert!(!phone.tree.contains_key("iTunes_Control/Music"));
    }

    #[tokio::test]
    async fn a_phone_without_itunes_control_gets_it_created() {
        let ran = run(FakePhone::new(&[]), true, false).await;
        ran.result.unwrap();
        for folder in [
            "iTunes_Control",
            "iTunes_Control/iTunes",
            "iTunes_Control/Sync",
        ] {
            assert_eq!(
                ran.phone.tree.get(folder),
                Some(&Node::Directory),
                "{folder}"
            );
        }
    }

    #[tokio::test]
    async fn a_retry_with_its_prefs_in_place_writes_no_prefs() {
        let prefs = prefs::adoption(0x1122, 0x3344, "rustunes").unwrap();
        let mut phone = FakePhone::new(&[
            "iTunes_Control",
            "iTunes_Control/iTunes",
            "iTunes_Control/Sync",
            "iTunes_Control/Sync/Media",
        ]);
        phone
            .tree
            .insert(PREFS.into(), Node::File(prefs.raw.clone()));
        phone
            .tree
            .insert(PREFS_PLIST.into(), Node::File(prefs.plist.clone()));
        let ran = run(phone, false, false).await;
        ran.result.unwrap();
        assert!(
            !ran.phone
                .log
                .contains(&idevice::afc::opcode::AfcOpcode::RenamePath)
        );
    }

    #[tokio::test]
    async fn existing_prefs_or_staged_files_stop_before_finish() {
        let mut theirs = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
        theirs
            .tree
            .insert(PREFS.into(), Node::File(b"theirs".to_vec()));
        let ran = run(theirs, true, false).await;
        assert!(ran.result.is_err());
        assert!(!ran.finished_sent);
        assert_eq!(file(&ran.phone, PREFS), b"theirs");
        assert!(!ran.phone.tree.contains_key(METADATA));

        let mut leftover = FakePhone::new(&[
            "iTunes_Control",
            "iTunes_Control/iTunes",
            "iTunes_Control/Sync",
            "iTunes_Control/Sync/Media",
        ]);
        leftover
            .tree
            .insert(SIGNATURE.to_owned(), Node::File(vec![0; 21]));
        let ran = run(leftover, false, false).await;
        assert!(ran.result.is_err());
        assert!(!ran.finished_sent);
    }

    const PREFS_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.tmp";
    const PREFS_PLIST_TEMPORARY: &str = "iTunes_Control/iTunes/iTunesPrefs.plist.tmp";

    fn synced_folders() -> FakePhone {
        FakePhone::new(&[
            "iTunes_Control",
            "iTunes_Control/iTunes",
            "iTunes_Control/Sync",
            "iTunes_Control/Sync/Media",
        ])
    }

    fn position(phone: &FakePhone, operation: idevice::afc::opcode::AfcOpcode) -> usize {
        phone.log.iter().position(|op| *op == operation).unwrap()
    }

    #[tokio::test]
    async fn a_retry_removes_its_cut_short_pair_and_stages_a_new_one() {
        use idevice::afc::opcode::AfcOpcode;
        let old_operation = b"<plist>the earlier attempt's operation</plist>".to_vec();
        let old_signature = [7_u8; 21];
        for (operation_kept, signature_kept) in
            [(0, None), (10, None), (old_operation.len(), Some(7))]
        {
            let mut phone = synced_folders();
            let prefs = adopted();
            phone
                .tree
                .insert(PREFS.into(), Node::File(prefs.raw.clone()));
            phone
                .tree
                .insert(PREFS_PLIST.into(), Node::File(prefs.plist.clone()));
            phone.tree.insert(
                METADATA.into(),
                Node::File(old_operation[..operation_kept].to_vec()),
            );
            if let Some(kept) = signature_kept {
                phone
                    .tree
                    .insert(SIGNATURE.into(), Node::File(old_signature[..kept].to_vec()));
            }
            let ran = run_with(
                phone,
                &[(SIGNATURE, &old_signature), (METADATA, &old_operation)],
                None,
                false,
            )
            .await;
            ran.result.unwrap();
            assert!(ran.finished_sent);
            let (operation, signature) = ran.persisted.unwrap();
            assert_eq!(file(&ran.phone, METADATA), operation);
            assert_eq!(file(&ran.phone, SIGNATURE), signature);
            // Removed before anything was written.
            assert!(
                position(&ran.phone, AfcOpcode::RemovePath)
                    < position(&ran.phone, AfcOpcode::Write)
            );
        }
    }

    #[tokio::test]
    async fn a_retry_after_the_first_prefs_file_writes_only_the_plist() {
        use idevice::afc::opcode::AfcOpcode;
        let prefs = adopted();
        for cut in [None, Some(0), Some(100), Some(prefs.plist.len())] {
            let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
            phone
                .tree
                .insert(PREFS.into(), Node::File(prefs.raw.clone()));
            let mut leftovers: Vec<(&str, &[u8])> = Vec::new();
            if let Some(cut) = cut {
                phone.tree.insert(
                    PREFS_PLIST_TEMPORARY.into(),
                    Node::File(prefs.plist[..cut].to_vec()),
                );
                leftovers.push((PREFS_PLIST_TEMPORARY, &prefs.plist));
            }
            let ran = run_with(phone, &leftovers, Some(&prefs), false).await;
            ran.result.unwrap();
            assert!(ran.finished_sent);
            assert_eq!(file(&ran.phone, PREFS), prefs.raw);
            assert_eq!(file(&ran.phone, PREFS_PLIST), prefs.plist);
            assert!(!ran.phone.tree.keys().any(|path| path.ends_with(".tmp")));
            let renames = ran
                .phone
                .log
                .iter()
                .filter(|op| **op == AfcOpcode::RenamePath)
                .count();
            assert_eq!(renames, 1, "{cut:?}");
        }
    }

    #[tokio::test]
    async fn a_retry_removes_its_cut_short_prefs_temporary_and_writes_both() {
        let prefs = adopted();
        for cut in [0, 12, prefs.raw.len()] {
            let mut phone = FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]);
            phone.tree.insert(
                PREFS_TEMPORARY.into(),
                Node::File(prefs.raw[..cut].to_vec()),
            );
            let ran = run_with(phone, &[(PREFS_TEMPORARY, &prefs.raw)], Some(&prefs), false).await;
            ran.result.unwrap();
            assert_eq!(file(&ran.phone, PREFS), prefs.raw);
            assert_eq!(file(&ran.phone, PREFS_PLIST), prefs.plist);
            assert!(!ran.phone.tree.keys().any(|path| path.ends_with(".tmp")));
        }
    }

    #[tokio::test]
    async fn leftovers_that_are_not_the_attempts_own_stop_before_anything_is_signed() {
        use idevice::afc::opcode::AfcOpcode;
        let prefs = adopted();
        // Bytes that are not the start of what the attempt was writing.
        let mut phone = synced_folders();
        phone
            .tree
            .insert(PREFS.into(), Node::File(prefs.raw.clone()));
        phone
            .tree
            .insert(PREFS_PLIST.into(), Node::File(prefs.plist.clone()));
        phone
            .tree
            .insert(METADATA.into(), Node::File(b"<plist>another".to_vec()));
        let ran = run_with(phone, &[(METADATA, b"<plist>ours</plist>")], None, false).await;
        let error = format!("{:#}", ran.result.unwrap_err());
        assert!(error.contains("nothing removed"), "{error}");
        assert!(ran.persisted.is_none());
        assert!(!ran.finished_sent);
        assert_eq!(file(&ran.phone, METADATA), b"<plist>another");
        assert!(!ran.phone.log.contains(&AfcOpcode::RemovePath));
        assert!(!ran.phone.log.contains(&AfcOpcode::Write));

        // A path a first session never leaves is never removed.
        let mut phone = synced_folders();
        let other = "iTunes_Control/iTunes/iTunesCDB";
        phone.tree.insert(other.into(), Node::File(Vec::new()));
        let ran = run_with(phone, &[(other, b"")], Some(&prefs), false).await;
        assert!(ran.result.is_err());
        assert!(ran.persisted.is_none());
        assert!(ran.phone.tree.contains_key(other));
        assert!(ran.phone.log.is_empty());
    }

    #[tokio::test]
    async fn a_stop_before_finish_leaves_the_pair_staged_and_sends_nothing_more() {
        let ran = run(
            FakePhone::new(&["iTunes_Control", "iTunes_Control/iTunes"]),
            true,
            true,
        )
        .await;
        assert!(ran.result.is_err());
        assert!(!ran.finished_sent);
        assert!(ran.phone.tree.contains_key(METADATA));
    }
}
