use super::*;

fn ready_at_140() -> Value {
    let mut ready = ready_message();
    set(
        &mut ready,
        &["Params", "DataclassAnchors", "Media"],
        "140".into(),
    );
    ready
}

#[tokio::test]
async fn ordinary_and_empty_delta_finish_preserve_distinct_anchor_and_metadata_modes() {
    for empty in [false, true] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let (request, finished) = references();
        let prepared = if empty {
            let prepared = prepare_empty_delta(request, &finished, ID, 140).unwrap();
            assert_eq!(
                child(
                    child(dictionary(prepared.request()).unwrap(), "Params").unwrap(),
                    "DataclassAnchors"
                )
                .unwrap()["Media"]
                    .as_string(),
                Some("140")
            );
            assert!(prepared.prepared.next_anchor().is_err());
            assert!(prepared.prepared.finished_message().is_err());
            prepared.prepared
        } else {
            let prepared = prepare(request, &finished, ID, 140).unwrap();
            assert_eq!(prepared.next_anchor().unwrap(), 141);
            prepared
        };
        let expected_request = prepared.request().clone();
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
            if empty {
                // Internal cross-mode calls must also refuse before any send.
                assert!(session.sign_metadata(b"must not sign").is_err());
                assert!(session.finish_metadata().await.is_err());
                assert!(session.phase == Phase::Ready);
                let mut session = EmptyDeltaSession { session };
                assert_eq!(
                    session.ready_receipt_sha256(),
                    <[u8; 32]>::from(Sha256::digest(wire(&ready_at_140())))
                );
                session.finish_empty_metadata().await.unwrap();
                assert!(session.session.phase == Phase::MetadataSent);
                assert!(session.finish_empty_metadata().await.is_err());
                assert!(session.session.sign_metadata(b"late").is_err());
                assert!(session.session.finish_metadata().await.is_err());
            } else {
                // Even an internally forged wrapper cannot turn a revision
                // preparation into unchanged-anchor empty completion.
                let mut wrong_mode = EmptyDeltaSession { session };
                assert!(wrong_mode.finish_empty_metadata().await.is_err());
                assert!(wrong_mode.session.phase == Phase::Ready);
                let mut session = wrong_mode.session;
                assert_eq!(session.sign_metadata(b"ordinary delta").unwrap().len(), 21);
                session.finish_metadata().await.unwrap();
                assert!(session.finish_metadata().await.is_err());
            }
        };
        let device = async {
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("Ping")
            );
            startup(&mut peer).await;
            assert_eq!(outgoing(&mut peer).await.0, expected_request);
            peer.write_all(&wire(&ready_at_140())).await.unwrap();
            assert_eq!(
                outgoing(&mut peer).await.0,
                dict(&[
                    ("Session", 2u64.into()),
                    ("Command", "FinishedSyncingMetadata".into()),
                    (
                        "Params",
                        dict(&[
                            (
                                "DataclassAnchors",
                                dict(&[
                                    ("Media", if empty { "140" } else { "141" }.into()),
                                    ("VoiceMemo", "1".into()),
                                ]),
                            ),
                            (
                                "SyncTypes",
                                dict(&[
                                    ("Media", 1u64.into()),
                                    ("Book", 0u64.into()),
                                    ("Keybag", 1u64.into()),
                                ]),
                            ),
                        ]),
                    ),
                ])
            );
            // Wrong-mode and repeated calls above emitted no extra frame.
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
    }
}

#[tokio::test]
async fn empty_delta_wrong_ready_refuses_before_any_finished_message() {
    for (media, session) in [("139", 2u64), ("141", 2), ("140", 99)] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let (request, finished) = references();
        let prepared = prepare_empty_delta(request, &finished, ID, 140).unwrap();
        let expected_request = prepared.request().clone();
        let host = async {
            let now = Instant::now();
            assert!(
                Session::begin(
                    client,
                    prepared.prepared,
                    &trace.0,
                    now + Duration::from_secs(2),
                    now + Duration::from_secs(1),
                )
                .await
                .is_err()
            );
        };
        let device = async {
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("Ping")
            );
            startup(&mut peer).await;
            assert_eq!(outgoing(&mut peer).await.0, expected_request);
            let mut ready = ready_at_140();
            set(
                &mut ready,
                &["Params", "DataclassAnchors", "Media"],
                media.into(),
            );
            set(&mut ready, &["Session"], session.into());
            peer.write_all(&wire(&ready)).await.unwrap();
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
    }
}

#[test]
fn empty_delta_reference_rejects_reset_changed_voice_wrong_library_and_payload() {
    for change in ["reset", "voice", "library", "payload", "old-anchor"] {
        let (mut request, mut finished) = references();
        let mut anchor = 140;
        match change {
            "reset" => set(
                &mut finished,
                &["Params", "SyncTypes", "Media"],
                2u64.into(),
            ),
            "voice" => set(
                &mut finished,
                &["Params", "DataclassAnchors", "VoiceMemo"],
                "2".into(),
            ),
            "library" => set(
                &mut request,
                &["Params", "HostInfo", "LibraryID"],
                "0000000000000004".into(),
            ),
            "payload" => set(
                &mut finished,
                &["Params", "Operations"],
                array(&["forbidden"]),
            ),
            "old-anchor" => anchor = 10,
            _ => unreachable!(),
        }
        assert!(prepare_empty_delta(request, &finished, ID, anchor).is_err());
    }
}
