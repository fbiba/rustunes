use super::*;

fn observation_ready() -> Value {
    let mut ready = ready_message();
    set(
        &mut ready,
        &["Params", "DataclassAnchors", "Media"],
        "140".into(),
    );
    ready
}
fn observation_prepared() -> Prepared {
    let (request, finished) = references();
    prepare_empty_delta(request, &finished, ID, 140)
        .unwrap()
        .prepared
}

async fn observe_transcript(messages: Vec<Value>, expected: Option<EmptyDeltaResponseKind>) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let prepared = observation_prepared();
    let request = prepared.request().clone();
    let final_message = messages.last().cloned();
    let host = async {
        let now = Instant::now();
        let session = Session::begin(
            client,
            prepared,
            &trace.0,
            now + Duration::from_secs(2),
            now + Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut session = EmptyDeltaSession { session };
        session.finish_empty_metadata().await.unwrap();
        let result = session.observe_after_empty_metadata().await;
        if let Some(kind) = expected {
            let observed = result.unwrap();
            assert_eq!(observed.kind(), kind);
            assert_eq!(observed.message(), final_message.as_ref().unwrap());
            assert_eq!(observed.telemetry_messages(), messages.len() - 1);
            let bytes = std::fs::read(
                trace
                    .0
                    .join(format!("{:03}-received.atc", observed.frame_index())),
            )
            .unwrap();
            assert_eq!(bytes, wire(observed.message()));
            assert_eq!(
                observed.frame_sha256(),
                <[u8; 32]>::from(Sha256::digest(&bytes))
            );
        } else {
            assert!(result.is_err());
            let expected = wire(final_message.as_ref().unwrap());
            let matching = std::fs::read_dir(&trace.0)
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.unwrap().path();
                    if path.to_string_lossy().ends_with("-received.atc") {
                        Some(std::fs::read(path).unwrap() == expected)
                    } else {
                        None
                    }
                })
                .filter(|matches| *matches)
                .count();
            assert!(matching > 0, "actual complete raw response was retained");
            if messages.len() == 64 {
                assert_eq!(matching, 64);
            }
        }
    };
    let device = async {
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("Ping")
        );
        startup(&mut peer).await;
        assert_eq!(outgoing(&mut peer).await.0, request);
        peer.write_all(&wire(&observation_ready())).await.unwrap();
        let finished = outgoing(&mut peer).await.0;
        assert_eq!(
            finished.as_dictionary().unwrap()["Command"].as_string(),
            Some("FinishedSyncingMetadata")
        );
        assert_eq!(
            child(
                child(dictionary(&finished).unwrap(), "Params").unwrap(),
                "DataclassAnchors"
            )
            .unwrap()["Media"]
                .as_string(),
            Some("140")
        );
        for value in &messages {
            peer.write_all(&wire(value)).await.unwrap();
        }
        // The observer consumes/closes its transport. No index/asset/error/
        // completion/terminal response or second finish is emitted.
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    };
    tokio::join!(host, device);
}

#[tokio::test]
async fn empty_observer_retains_actual_manifest_terminal_refusal_and_unknown_without_replies() {
    let manifest = incoming(
        "AssetManifest",
        Some(combined_manifest(&[("-12345", 5), ("-888", 2)])),
    );
    observe_transcript(vec![manifest], Some(EmptyDeltaResponseKind::AssetManifest)).await;
    for params in [None, Some(dict(&[]))] {
        observe_transcript(
            vec![incoming("SyncFinished", params)],
            Some(EmptyDeltaResponseKind::SyncFinished),
        )
        .await;
    }
    observe_transcript(
        vec![incoming(
            "SyncFailed",
            Some(dict(&[("ErrorCode", 16u64.into())])),
        )],
        Some(EmptyDeltaResponseKind::SyncFailed),
    )
    .await;
    // This synthetic unknown name is not evidence of a real index opcode.
    observe_transcript(
        vec![incoming(
            "FullAudioIndex",
            Some(dict(&[("Dataclass", "Media".into())])),
        )],
        Some(EmptyDeltaResponseKind::UnsupportedCommand),
    )
    .await;
    observe_transcript(
        vec![incoming(
            "UnmappedIndexRequest",
            Some(Value::Data(vec![1, 2, 3])),
        )],
        Some(EmptyDeltaResponseKind::UnsupportedCommand),
    )
    .await;
}

#[tokio::test]
async fn empty_observer_checks_telemetry_then_stops_at_the_first_nontelemetry() {
    observe_transcript(
        vec![
            incoming(
                "Progress",
                Some(dict(&[("OverallProgress", 0.5f64.into())])),
            ),
            allowed(),
            incoming("InstalledAssets", Some(dict(&[("Opaque", true.into())]))),
            incoming("AssetMetrics", Some(dict(&[]))),
            incoming("AssetManifest", Some(combined_manifest(&[("-12345", 5)]))),
        ],
        Some(EmptyDeltaResponseKind::AssetManifest),
    )
    .await;
}

#[tokio::test]
async fn empty_observer_refuses_wrong_envelopes_and_bad_telemetry_after_retaining_raw() {
    for change in ["session", "type", "extra", "params", "progress"] {
        let mut value = incoming("AssetManifest", Some(combined_manifest(&[("-12345", 5)])));
        match change {
            "session" => set(&mut value, &["Session"], 99u64.into()),
            "type" => set(&mut value, &["Type"], 1u64.into()),
            "extra" => set(&mut value, &["Unexpected"], true.into()),
            "params" => set(&mut value, &["Params"], "not a dictionary".into()),
            "progress" => {
                value = incoming(
                    "Progress",
                    Some(dict(&[("OverallProgress", 2.0f64.into())])),
                )
            }
            _ => unreachable!(),
        }
        observe_transcript(vec![value], None).await;
    }
}

#[tokio::test]
async fn empty_observer_phase_mode_and_64_frame_bound_close_without_extra_sends() {
    observe_transcript(
        (0..64)
            .map(|_| {
                incoming(
                    "Progress",
                    Some(dict(&[("OverallProgress", 0.5f64.into())])),
                )
            })
            .collect(),
        None,
    )
    .await;
    for wrong_mode in [false, true] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let prepared = if wrong_mode {
            let (request, finished) = references();
            prepare(request, &finished, ID, 140).unwrap()
        } else {
            observation_prepared()
        };
        let request = prepared.request().clone();
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
            if wrong_mode {
                session.phase = Phase::MetadataSent;
            }
            assert!(
                EmptyDeltaSession { session }
                    .observe_after_empty_metadata()
                    .await
                    .is_err()
            );
        };
        let device = async {
            outgoing(&mut peer).await;
            startup(&mut peer).await;
            assert_eq!(outgoing(&mut peer).await.0, request);
            peer.write_all(&wire(&observation_ready())).await.unwrap();
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
    }
}

#[tokio::test(start_paused = true)]
async fn empty_observer_partial_eof_and_timeout_are_incomplete_not_stale_ready_receipts() {
    for timeout in [false, true] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let prepared = observation_prepared();
        let host = async {
            let now = Instant::now();
            let session = Session::begin(
                client,
                prepared,
                &trace.0,
                now + Duration::from_millis(300),
                now + Duration::from_millis(200),
            )
            .await
            .unwrap();
            let mut session = EmptyDeltaSession { session };
            session.finish_empty_metadata().await.unwrap();
            let frame_index = session.session.channel.frames;
            assert!(session.observe_after_empty_metadata().await.is_err());
            let partial =
                std::fs::read(trace.0.join(format!("{:03}-partial.atc", frame_index))).unwrap();
            assert_eq!(
                partial,
                [16u32.to_le_bytes().as_slice(), b"bplist00"].concat()
            );
            assert!(
                !trace
                    .0
                    .join(format!("{:03}-received.atc", frame_index))
                    .exists()
            );
        };
        let device = async {
            outgoing(&mut peer).await;
            startup(&mut peer).await;
            outgoing(&mut peer).await;
            peer.write_all(&wire(&observation_ready())).await.unwrap();
            outgoing(&mut peer).await;
            peer.write_all(&[16u32.to_le_bytes().as_slice(), b"bplist00"].concat())
                .await
                .unwrap();
            if !timeout {
                peer.shutdown().await.unwrap();
            }
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
    }
}
