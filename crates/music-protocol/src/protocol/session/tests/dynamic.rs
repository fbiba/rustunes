use super::*;

// Included in the existing synthetic-transport test module to reuse its peers.
async fn dynamic_audio_transcript(message: Value, declined: &[&str], valid: bool, repeated: bool) {
    dynamic_audio_transcript_profile(
        message,
        declined,
        valid,
        repeated,
        crate::protocol::ancillary::AudioManifestProfile::AudioOnly,
    )
    .await;
}

async fn dynamic_audio_transcript_profile(
    message: Value,
    declined: &[&str],
    valid: bool,
    repeated: bool,
    profile: crate::protocol::ancillary::AudioManifestProfile,
) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let snapshot = crate::protocol::ancillary::tests::synthetic(ID, 11);
    let policy = match profile {
        crate::protocol::ancillary::AudioManifestProfile::AudioOnly => snapshot.audio("-12345"),
        crate::protocol::ancillary::AudioManifestProfile::AudioWithDisabledGenius => {
            snapshot.audio_with_disabled_genius("-12345")
        }
        crate::protocol::ancillary::AudioManifestProfile::AudioWithArtwork => {
            snapshot.audio_batch_with_artwork(&[("-12345".to_owned(), true)])
        }
    }
    .unwrap();
    assert_eq!(policy.manifest_profile(), profile);
    let host = async {
        let now = Instant::now();
        let mut session = Session::begin(
            client,
            prepared(),
            &trace.0,
            now + Duration::from_secs(2),
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        session.finish_metadata().await.unwrap();
        let wait = session.wait_for_audio_with_policy(&policy).await;
        if valid {
            wait.unwrap();
            assert!(session.wait_for_audio_with_policy(&policy).await.is_err());
            assert!(session.complete_artwork("-12345").await.is_err());
            assert!(
                session
                    .complete_audio("999", "iTunes_Control/Music/F00/ABCD.m4a")
                    .await
                    .is_err()
            );
            session
                .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.m4a")
                .await
                .unwrap();
            assert_eq!(session.wait_finished().await.is_ok(), !repeated);
        } else {
            assert!(wait.is_err());
            assert!(
                session
                    .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.m4a")
                    .await
                    .is_err()
            );
            assert!(session.complete_artwork("-12345").await.is_err());
            assert!(session.wait_for_audio_with_policy(&policy).await.is_err());
            assert!(session.wait_for_audio("-12345", None).await.is_err());
            assert!(session.wait_for_artwork("-12345", None).await.is_err());
        }
    };
    let device = async {
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("Ping")
        );
        startup(&mut peer).await;
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("RequestingSync")
        );
        peer.write_all(&wire(&ready_message())).await.unwrap();
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("FinishedSyncingMetadata")
        );
        peer.write_all(&wire(&message)).await.unwrap();
        if valid {
            assert_eq!(
                outgoing(&mut peer).await.0,
                dict(&[
                    ("Session", 2u64.into()),
                    ("Command", "FileComplete".into()),
                    (
                        "Params",
                        dict(&[
                            ("AssetID", "-12345".into()),
                            ("AssetPath", "iTunes_Control/Music/F00/ABCD.m4a".into()),
                            ("Dataclass", "Media".into())
                        ])
                    ),
                ])
            );
            for id in declined {
                assert_eq!(
                    outgoing(&mut peer).await.0,
                    dict(&[
                        ("Session", 2u64.into()),
                        ("Command", "FileError".into()),
                        (
                            "Params",
                            dict(&[
                                ("AssetID", (*id).into()),
                                ("Dataclass", "Media".into()),
                                ("ErrorCode", 16i64.into()),
                                ("ErrorIsVisible", false.into())
                            ])
                        ),
                    ])
                );
            }
            peer.write_all(&wire(&if repeated {
                message.clone()
            } else {
                incoming("SyncFinished", None)
            }))
            .await
            .unwrap();
        }
        // Malformed full manifests and repeated requests cause no replies.
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    };
    tokio::join!(host, device);
}

#[tokio::test]
async fn dynamic_audio_completes_own_before_fresh_foreign_refusals_and_terminal() {
    dynamic_audio_transcript(
        crate::protocol::ancillary::tests::manifest(&[("-888", 2), ("-12345", 1), ("999", 4)]),
        &["-888", "999"],
        true,
        false,
    )
    .await;
    dynamic_audio_transcript(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 1)]),
        &[],
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn dynamic_audio_rejects_whole_invalid_manifest_without_sends_or_legacy_fallback() {
    for records in [
        vec![],
        vec![("-888", 2)],
        vec![("-888", 2), ("-12345", 1), ("999", 2)],
        vec![("-888", 2), ("-12345", 1), ("unknown", 4)],
        vec![("-12345", 3)],
        vec![("-12345", 1), ("-12345", 1)],
    ] {
        dynamic_audio_transcript(
            crate::protocol::ancillary::tests::manifest(&records),
            &[],
            false,
            false,
        )
        .await;
    }
    let mut wrong = crate::protocol::ancillary::tests::manifest(&[("-12345", 1)]);
    set(&mut wrong, &["Session"], 3u64.into());
    dynamic_audio_transcript(wrong, &[], false, false).await;
}

#[tokio::test]
async fn dynamic_audio_repeated_manifest_is_not_answered_or_accepted_as_terminal() {
    dynamic_audio_transcript(
        crate::protocol::ancillary::tests::manifest(&[("-888", 2), ("-12345", 1), ("999", 4)]),
        &["-888", "999"],
        true,
        true,
    )
    .await;
}

#[tokio::test]
async fn dynamic_audio_genius_accepts_observed_mask_without_own_refusal() {
    // Synthetic bound identities matching an observed insert's wire shape.
    dynamic_audio_transcript_profile(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 5), ("-888", 2)]),
        &["-888"],
        true,
        false,
        crate::protocol::ancillary::AudioManifestProfile::AudioWithDisabledGenius,
    )
    .await;
}

#[tokio::test]
async fn dynamic_audio_with_artwork_accepts_track_and_artwork_mask() {
    // The reference host's insertion with artwork: one own request for Track
    // and Artwork (mask 3), completed once with the audio path.
    dynamic_audio_transcript_profile(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 3), ("-888", 2)]),
        &["-888"],
        true,
        false,
        crate::protocol::ancillary::AudioManifestProfile::AudioWithArtwork,
    )
    .await;
    dynamic_audio_transcript_profile(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 1), ("-888", 2)]),
        &[],
        false,
        false,
        crate::protocol::ancillary::AudioManifestProfile::AudioWithArtwork,
    )
    .await;
}

#[tokio::test]
async fn dynamic_audio_profiles_preserve_audio_only_refusal_and_genius_ordinary_audio() {
    dynamic_audio_transcript(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 5), ("-888", 2)]),
        &[],
        false,
        false,
    )
    .await;
    dynamic_audio_transcript_profile(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 1), ("-888", 2)]),
        &["-888"],
        true,
        false,
        crate::protocol::ancillary::AudioManifestProfile::AudioWithDisabledGenius,
    )
    .await;
}

#[tokio::test]
async fn dynamic_audio_genius_validates_whole_manifest_before_asset_responses() {
    for part in [1, 5] {
        for records in [
            vec![("-12345", part), ("-12345", 1)],
            vec![("-12345", part), ("-888", 5)],
            vec![("-12345", part), ("-888", 1)],
            vec![("-12345", part), ("unknown", 4)],
            vec![("-12345", part), ("-888", 2), ("999", 2)],
        ] {
            dynamic_audio_transcript_profile(
                crate::protocol::ancillary::tests::manifest(&records),
                &[],
                false,
                false,
                crate::protocol::ancillary::AudioManifestProfile::AudioWithDisabledGenius,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn dynamic_audio_genius_repeated_manifest_is_not_terminal_or_answered() {
    dynamic_audio_transcript_profile(
        crate::protocol::ancillary::tests::manifest(&[("-12345", 5), ("-888", 2)]),
        &["-888"],
        true,
        true,
        crate::protocol::ancillary::AudioManifestProfile::AudioWithDisabledGenius,
    )
    .await;
}

#[tokio::test]
async fn dynamic_artwork_uses_existing_target_empty_path_and_rejects_audio_or_repeat() {
    for own_part in [1, 2, 3] {
        for repeated in [false, true] {
            let trace = Temporary::new();
            let (client, mut peer) = transport(MAX_BYTES).await;
            let snapshot = crate::protocol::ancillary::tests::synthetic(ID, 11);
            let policy = snapshot.artwork("-1").unwrap();
            let host = async {
                let now = Instant::now();
                let mut session = Session::begin(
                    client,
                    prepared(),
                    &trace.0,
                    now + Duration::from_secs(2),
                    now + Duration::from_secs(1),
                )
                .await
                .unwrap();
                session.finish_metadata().await.unwrap();
                let wait = session.wait_for_artwork_with_policy(&policy).await;
                assert_eq!(wait.is_ok(), own_part == 2);
                assert!(
                    session
                        .complete_audio("-1", "iTunes_Control/Music/F00/ABCD.m4a")
                        .await
                        .is_err()
                );
                assert!(session.wait_for_artwork_with_policy(&policy).await.is_err());
                if own_part == 2 {
                    session.complete_artwork("-1").await.unwrap();
                    assert_eq!(session.wait_finished().await.is_ok(), !repeated);
                } else {
                    assert!(session.complete_artwork("-1").await.is_err());
                    assert!(session.wait_for_artwork("-1", None).await.is_err());
                }
            };
            let device = async {
                assert_eq!(
                    outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                    Some("Ping")
                );
                startup(&mut peer).await;
                assert_eq!(
                    outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                    Some("RequestingSync")
                );
                peer.write_all(&wire(&ready_message())).await.unwrap();
                assert_eq!(
                    outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                    Some("FinishedSyncingMetadata")
                );
                let message = crate::protocol::ancillary::tests::manifest(&[
                    ("-888", 2),
                    ("-1", own_part),
                    ("999", 4),
                ]);
                peer.write_all(&wire(&message)).await.unwrap();
                if own_part == 2 {
                    assert_eq!(
                        outgoing(&mut peer).await.0,
                        dict(&[
                            ("Session", 2u64.into()),
                            ("Command", "FileComplete".into()),
                            (
                                "Params",
                                dict(&[
                                    ("AssetID", "-1".into()),
                                    ("AssetPath", "".into()),
                                    ("Dataclass", "Media".into())
                                ])
                            ),
                        ])
                    );
                    for id in ["-888", "999"] {
                        assert_eq!(
                            outgoing(&mut peer).await.0,
                            dict(&[
                                ("Session", 2u64.into()),
                                ("Command", "FileError".into()),
                                (
                                    "Params",
                                    dict(&[
                                        ("AssetID", id.into()),
                                        ("Dataclass", "Media".into()),
                                        ("ErrorCode", 16i64.into()),
                                        ("ErrorIsVisible", false.into())
                                    ])
                                ),
                            ])
                        );
                    }
                    peer.write_all(&wire(&if repeated {
                        message
                    } else {
                        incoming("SyncFinished", None)
                    }))
                    .await
                    .unwrap();
                }
                let mut byte = [0];
                assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
            };
            tokio::join!(host, device);
        }
    }
}

async fn metadata_dynamic_transcript(messages: Vec<Value>, errors: &[&str], success: bool) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let snapshot = crate::protocol::ancillary::tests::synthetic(ID, 11);
    let policy = snapshot.metadata("-1").unwrap();
    let host = async {
        let now = Instant::now();
        let mut session = Session::begin(
            client,
            prepared(),
            &trace.0,
            now + Duration::from_secs(2),
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        session.finish_metadata().await.unwrap();
        assert_eq!(
            session
                .wait_metadata_only_with_policy(&policy)
                .await
                .is_ok(),
            success
        );
        // Both terminal and error consume this flow. A caller cannot turn
        // an unexpected own request into an upload or retry a late wait.
        assert!(
            session
                .wait_metadata_only_with_policy(&policy)
                .await
                .is_err()
        );
        assert!(session.wait_for_audio("-12345", None).await.is_err());
        assert!(session.complete_artwork("-12345").await.is_err());
        assert!(session.wait_finished().await.is_err());
    };
    let device = async {
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("Ping")
        );
        startup(&mut peer).await;
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("RequestingSync")
        );
        peer.write_all(&wire(&ready_message())).await.unwrap();
        let end = outgoing(&mut peer).await.0;
        assert_eq!(end, prepared().finished_message().unwrap());
        for message in messages {
            peer.write_all(&wire(&message)).await.unwrap();
        }
        for id in errors {
            assert_eq!(
                outgoing(&mut peer).await.0,
                dict(&[
                    ("Session", 2u64.into()),
                    ("Command", "FileError".into()),
                    (
                        "Params",
                        dict(&[
                            ("AssetID", (*id).into()),
                            ("Dataclass", "Media".into()),
                            ("ErrorCode", Value::Integer(16i64.into())),
                            ("ErrorIsVisible", false.into())
                        ])
                    ),
                ])
            );
        }
        let mut remaining = Vec::new();
        peer.read_to_end(&mut remaining).await.unwrap();
        assert!(
            remaining.is_empty(),
            "unexpected outgoing reply or synthetic FileComplete"
        );
    };
    tokio::join!(host, device);
}

#[tokio::test]
async fn dynamic_metadata_only_validates_foreign_requests_and_never_completes_files() {
    for records in [vec![], vec![("-888", 2)], vec![("999", 4), ("-888", 2)]] {
        let errors: Vec<_> = records.iter().map(|(id, _)| *id).collect();
        metadata_dynamic_transcript(
            vec![
                crate::protocol::ancillary::tests::manifest(&records),
                incoming("SyncFinished", None),
            ],
            &errors,
            true,
        )
        .await;
    }
    metadata_dynamic_transcript(vec![incoming("SyncFinished", None)], &[], true).await;
    for records in [
        vec![("-888", 2), ("-1", 1)],
        vec![("-888", 2), ("-1", 2)],
        vec![("-888", 2), ("999", 2)],
        vec![("-888", 2), ("77", 4)],
        vec![("-888", 2), ("-888", 2)],
    ] {
        metadata_dynamic_transcript(
            vec![crate::protocol::ancillary::tests::manifest(&records)],
            &[],
            false,
        )
        .await;
    }
    let manifest = crate::protocol::ancillary::tests::manifest(&[("-888", 2)]);
    metadata_dynamic_transcript(vec![manifest.clone(), manifest.clone()], &["-888"], false).await;
    let mut wrong = manifest;
    set(&mut wrong, &["Session"], 3u64.into());
    metadata_dynamic_transcript(vec![wrong], &[], false).await;
    metadata_dynamic_transcript(
        vec![incoming("SyncFinished", Some(true.into()))],
        &[],
        false,
    )
    .await;
}

async fn pending_delete_transcript(messages: Vec<Value>, errors: &[&str], success: bool) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let (catalog, identity, snapshot, deletion) =
        crate::protocol::ancillary::pending_delete::tests::session_fixture(ID, 11);
    let policy = snapshot
        .pending_delete(&catalog, &identity, &deletion)
        .unwrap();
    let host = async {
        let now = Instant::now();
        let mut session = Session::begin(
            client,
            prepared(),
            &trace.0,
            now + Duration::from_secs(2),
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        // The wait belongs to the metadata phase only.
        assert!(session.wait_pending_delete(&policy).await.is_err());
        session.finish_metadata().await.unwrap();
        assert_eq!(session.wait_pending_delete(&policy).await.is_ok(), success);
        // Terminal and error both consume the flow; nothing can continue it.
        assert!(session.wait_pending_delete(&policy).await.is_err());
        assert!(session.wait_for_audio("-1", None).await.is_err());
        assert!(
            session
                .complete_audio("-1", "iTunes_Control/Music/F39/OMJJ.m4a")
                .await
                .is_err()
        );
        assert!(session.wait_finished().await.is_err());
        assert_eq!(session.terminal_receipt_sha256().is_ok(), success);
    };
    let device = async {
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("Ping")
        );
        startup(&mut peer).await;
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("RequestingSync")
        );
        peer.write_all(&wire(&ready_message())).await.unwrap();
        let end = outgoing(&mut peer).await.0;
        assert_eq!(end, prepared().finished_message().unwrap());
        for message in messages {
            peer.write_all(&wire(&message)).await.unwrap();
        }
        for id in errors {
            assert_eq!(
                outgoing(&mut peer).await.0,
                dict(&[
                    ("Session", 2u64.into()),
                    ("Command", "FileError".into()),
                    (
                        "Params",
                        dict(&[
                            ("AssetID", (*id).into()),
                            ("Dataclass", "Media".into()),
                            ("ErrorCode", Value::Integer(16i64.into())),
                            ("ErrorIsVisible", false.into())
                        ])
                    ),
                ])
            );
        }
        let mut remaining = Vec::new();
        peer.read_to_end(&mut remaining).await.unwrap();
        assert!(
            remaining.is_empty(),
            "unexpected outgoing reply or synthetic FileComplete"
        );
    };
    tokio::join!(host, device);
}

#[tokio::test]
async fn pending_delete_wait_refuses_foreign_assets_and_ends_on_own_request() {
    let manifest = crate::protocol::ancillary::tests::manifest;
    pending_delete_transcript(vec![incoming("SyncFinished", None)], &[], true).await;
    for part in [2, 4] {
        pending_delete_transcript(
            vec![manifest(&[("-2", part)]), incoming("SyncFinished", None)],
            &["-2"],
            true,
        )
        .await;
    }
    // Any request for the removed row's own asset, alone or after a valid
    // foreign request, ends the session before any reply.
    for records in [
        vec![("-1", 1)],
        vec![("-1", 2)],
        vec![("-1", 0)],
        vec![("-2", 2), ("-1", 1)],
        vec![("-2", 1)],
        vec![("77", 4)],
    ] {
        pending_delete_transcript(vec![manifest(&records)], &[], false).await;
    }
    let repeated = manifest(&[("-2", 4)]);
    pending_delete_transcript(vec![repeated.clone(), repeated], &["-2"], false).await;
    let mut wrong = manifest(&[]);
    set(&mut wrong, &["Session"], 3u64.into());
    pending_delete_transcript(vec![wrong], &[], false).await;
    pending_delete_transcript(vec![incoming("FileComplete", None)], &[], false).await;
}

#[tokio::test]
async fn dynamic_audio_batch_completes_every_own_asset_before_refusals_and_terminal() {
    let own = ["-12345".to_owned(), "-12346".to_owned()];
    for (records, valid) in [
        (vec![("-888", 2), ("-12346", 1), ("-12345", 1)], true),
        // Every own asset must be requested, each exactly once.
        (vec![("-12345", 1), ("-888", 2)], false),
        (vec![("-12345", 1), ("-12346", 1), ("-12346", 1)], false),
    ] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let snapshot = crate::protocol::ancillary::tests::synthetic(ID, 11);
        let policy = snapshot.audio_batch(&own).unwrap();
        let message = crate::protocol::ancillary::tests::manifest(&records);
        let host = async {
            let now = Instant::now();
            let mut session = Session::begin(
                client,
                prepared(),
                &trace.0,
                now + Duration::from_secs(2),
                now + Duration::from_secs(1),
            )
            .await
            .unwrap();
            session.finish_metadata().await.unwrap();
            let wait = session.wait_for_audio_with_policy(&policy).await;
            if !valid {
                assert!(wait.is_err());
                return;
            }
            wait.unwrap();
            session
                .complete_audio("-12346", "iTunes_Control/Music/F00/ABCE.m4a")
                .await
                .unwrap();
            // One own asset is still outstanding: no terminal wait yet, and a
            // repeated completion is refused.
            assert!(session.wait_finished().await.is_err());
            assert!(
                session
                    .complete_audio("-12346", "iTunes_Control/Music/F00/ABCE.m4a")
                    .await
                    .is_err()
            );
            session
                .complete_audio("-12345", "iTunes_Control/Music/F00/ABCD.m4a")
                .await
                .unwrap();
            session.wait_finished().await.unwrap();
        };
        let device = async {
            for expected in ["Ping"] {
                assert_eq!(
                    outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                    Some(expected)
                );
            }
            startup(&mut peer).await;
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("RequestingSync")
            );
            peer.write_all(&wire(&ready_message())).await.unwrap();
            assert_eq!(
                outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
                Some("FinishedSyncingMetadata")
            );
            peer.write_all(&wire(&message)).await.unwrap();
            if valid {
                let mut sent = Vec::new();
                for _ in 0..3 {
                    let value = outgoing(&mut peer).await.0;
                    let root = value.as_dictionary().unwrap();
                    let params = root["Params"].as_dictionary().unwrap();
                    sent.push((
                        root["Command"].as_string().unwrap().to_owned(),
                        params["AssetID"].as_string().unwrap().to_owned(),
                    ));
                }
                assert_eq!(
                    sent,
                    [
                        ("FileComplete".to_owned(), "-12346".to_owned()),
                        ("FileComplete".to_owned(), "-12345".to_owned()),
                        ("FileError".to_owned(), "-888".to_owned()),
                    ]
                );
                peer.write_all(&wire(&incoming("SyncFinished", None)))
                    .await
                    .unwrap();
            }
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
    }
}
