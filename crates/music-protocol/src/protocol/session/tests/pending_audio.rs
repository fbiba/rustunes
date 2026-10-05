use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn pending_control(duration: Duration) -> crate::protocol::transfer::CaptureControl {
    crate::protocol::transfer::CaptureControl {
        deadline: Instant::now() + duration,
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}

fn pending_prepared() -> Prepared {
    let (request, finished) = references();
    prepare_empty_delta(request, &finished, ID, 140)
        .unwrap()
        .prepared
}
fn pending_ready() -> Value {
    let mut ready = ready_message();
    set(
        &mut ready,
        &["Params", "DataclassAnchors", "Media"],
        "140".into(),
    );
    ready
}
async fn pending_transcript(response: Value, accepted: bool) {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let (catalog, identity, snapshot) = crate::protocol::ancillary::tests::pending_fixture(140);
    let policy = snapshot
        .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
        .unwrap();
    let control = pending_control(Duration::from_secs(2));
    let prepared = pending_prepared();
    let request = prepared.request().clone();
    // Bounded in-memory control pipes; reuse existing io-util features.
    let (mut release, mut released) = tokio::io::duplex(1);
    let (mut noticed, mut notice) = tokio::io::duplex(1);
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
        let result = session.observe_pending_audio(&policy, &control).await;
        assert_eq!(result.is_ok(), accepted);
        if accepted {
            let mut demand = result.unwrap();
            assert_eq!(demand.own_asset_id(), "-1");
            assert_eq!(demand.observation().message(), &response);
            let raw = std::fs::read(trace.0.join(format!(
                "{:03}-received.atc",
                demand.observation().frame_index()
            )))
            .unwrap();
            assert_eq!(
                demand.observation().frame_sha256(),
                <[u8; 32]>::from(Sha256::digest(&raw))
            );
            // Even internal access cannot use the ordinary accepted-asset phase.
            assert!(
                demand
                    ._session
                    .complete_audio("-1", "iTunes_Control/Music/F39/TEST.m4a")
                    .await
                    .is_err()
            );
            assert!(demand._session.sign_metadata(b"no metadata").is_err());
            assert!(demand._session.finish_metadata().await.is_err());
            noticed.write_all(&[1]).await.unwrap();
            let mut byte = [0];
            released.read_exact(&mut byte).await.unwrap();
            drop(demand);
        }
    };
    let device = async {
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("Ping")
        );
        startup(&mut peer).await;
        assert_eq!(outgoing(&mut peer).await.0, request);
        peer.write_all(&wire(&pending_ready())).await.unwrap();
        let finish = outgoing(&mut peer).await.0;
        assert_eq!(
            finish.as_dictionary().unwrap()["Command"].as_string(),
            Some("FinishedSyncingMetadata")
        );
        assert_eq!(
            child(
                child(dictionary(&finish).unwrap(), "Params").unwrap(),
                "DataclassAnchors"
            )
            .unwrap()["Media"]
                .as_string(),
            Some("140")
        );
        peer.write_all(&wire(&response)).await.unwrap();
        let mut byte = [0];
        if accepted {
            notice.read_exact(&mut byte).await.unwrap();
            // The exact live connection remains held, without a single reply.
            assert!(
                timeout_at(
                    Instant::now() + Duration::from_millis(20),
                    peer.read(&mut byte)
                )
                .await
                .is_err()
            );
            release.write_all(&[1]).await.unwrap();
        }
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    };
    tokio::join!(host, device);
}

#[tokio::test]
async fn pending_demand_retains_same_live_transport_and_raw_manifest_without_reply() {
    for mask in [1, 5] {
        pending_transcript(
            incoming("AssetManifest", Some(combined_manifest(&[("-1", mask)]))),
            true,
        )
        .await;
    }
}

#[tokio::test]
async fn pending_demand_rejects_late_bad_foreign_terminal_unknown_and_wrong_session() {
    for response in [
        incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5), ("999", 2)])),
        ),
        incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5), ("-1", 1)])),
        ),
        incoming("SyncFinished", None),
        incoming("SyncFailed", Some(dict(&[]))),
        incoming("UnmappedIndexRequest", Some(Value::Data(vec![1, 2, 3]))),
    ] {
        pending_transcript(response, false).await;
    }
    let mut wrong = incoming("AssetManifest", Some(combined_manifest(&[("-1", 5)])));
    set(&mut wrong, &["Session"], 99u64.into());
    pending_transcript(wrong, false).await;
}

#[tokio::test]
async fn pending_demand_requires_successful_empty_finish_and_refuses_partial_eof() {
    for unfinished in [true, false] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let (catalog, identity, snapshot) = crate::protocol::ancillary::tests::pending_fixture(140);
        let policy = snapshot
            .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
            .unwrap();
        let control = pending_control(Duration::from_secs(2));
        let prepared = pending_prepared();
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
            if !unfinished {
                session.finish_empty_metadata().await.unwrap();
            }
            assert!(
                session
                    .observe_pending_audio(&policy, &control)
                    .await
                    .is_err()
            );
        };
        let device = async {
            outgoing(&mut peer).await;
            startup(&mut peer).await;
            outgoing(&mut peer).await;
            peer.write_all(&wire(&pending_ready())).await.unwrap();
            if unfinished {
                let mut byte = [0];
                assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
            } else {
                outgoing(&mut peer).await;
                let raw = wire(&incoming(
                    "AssetManifest",
                    Some(combined_manifest(&[("-1", 5)])),
                ));
                peer.write_all(&raw[..7]).await.unwrap();
                peer.shutdown().await.unwrap();
            }
        };
        tokio::join!(host, device);
        if !unfinished {
            assert!(std::fs::read_dir(&trace.0).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .path()
                    .to_string_lossy()
                    .ends_with("-partial.atc")
            }));
        }
    }
}

// Test-only observation of actual consumed bytes. No producer/permit factory.
#[derive(Debug)]
struct ReadMeter {
    inner: DuplexStream,
    consumed: Arc<AtomicUsize>,
    cancel_after: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
}
impl AsyncRead for ReadMeter {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = output.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, output);
        if matches!(&result, Poll::Ready(Ok(()))) {
            let read = output.filled().len() - before;
            let actual = this.consumed.fetch_add(read, Ordering::SeqCst) + read;
            if actual >= this.cancel_after.load(Ordering::SeqCst) {
                this.cancelled.store(true, Ordering::SeqCst);
            }
        }
        result
    }
}
impl AsyncWrite for ReadMeter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[tokio::test(start_paused = true)]
async fn pending_demand_control_retains_consumed_partial_and_rejected_complete_frames() {
    // Cases: cancellation with seven actually consumed bytes, caller deadline
    // earlier than Channel deadline with seven consumed bytes, then cancellation
    // at the complete read before CaptureControl's post-check.
    for case in 0..3 {
        let trace = Temporary::new();
        let control = pending_control(if case == 1 {
            Duration::from_millis(80)
        } else {
            Duration::from_secs(2)
        });
        let consumed = Arc::new(AtomicUsize::new(0));
        let cancel_after = Arc::new(AtomicUsize::new(usize::MAX));
        let (stream, mut peer) = tokio::io::duplex(64 * 1024);
        let client = budgeted(
            Idevice::new(
                Box::new(ReadMeter {
                    inner: stream,
                    consumed: Arc::clone(&consumed),
                    cancel_after: Arc::clone(&cancel_after),
                    cancelled: Arc::clone(&control.cancelled),
                }),
                "offline-only",
            ),
            MAX_BYTES,
        )
        .await
        .unwrap();
        let (catalog, identity, snapshot) = crate::protocol::ancillary::tests::pending_fixture(140);
        let policy = snapshot
            .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
            .unwrap();
        let response = incoming("AssetManifest", Some(combined_manifest(&[("-1", 5)])));
        let full = wire(&response);
        let sent = if case == 2 {
            full.clone()
        } else {
            full[..7].to_vec()
        };
        let host = async {
            let now = Instant::now();
            let session = Session::begin(
                client,
                pending_prepared(),
                &trace.0,
                now + Duration::from_secs(2),
                now + Duration::from_secs(1),
            )
            .await
            .unwrap();
            let mut session = EmptyDeltaSession { session };
            session.finish_empty_metadata().await.unwrap();
            assert!(
                session
                    .observe_pending_audio(&policy, &control)
                    .await
                    .is_err()
            );
        };
        let device = async {
            outgoing(&mut peer).await;
            startup(&mut peer).await;
            outgoing(&mut peer).await;
            peer.write_all(&wire(&pending_ready())).await.unwrap();
            outgoing(&mut peer).await;
            let before = consumed.load(Ordering::SeqCst);
            if case == 2 {
                cancel_after.store(before + sent.len(), Ordering::SeqCst);
            }
            peer.write_all(&sent).await.unwrap();
            while consumed.load(Ordering::SeqCst) < before + sent.len() {
                tokio::task::yield_now().await;
            }
            assert_eq!(consumed.load(Ordering::SeqCst) - before, sent.len());
            if case == 0 {
                control.cancelled.store(true, Ordering::SeqCst);
            }
            let mut byte = [0];
            // No reply, second finish or fabricated terminal; actual close.
            assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        };
        tokio::join!(host, device);
        let suffix = if case == 2 {
            "-received.atc"
        } else {
            "-partial.atc"
        };
        let matched: Vec<_> = std::fs::read_dir(&trace.0)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                (path.to_string_lossy().ends_with(suffix) && std::fs::read(&path).unwrap() == sent)
                    .then_some(path)
            })
            .collect();
        assert_eq!(
            matched.len(),
            1,
            "retained actual consumed bytes, never old Ready digest"
        );
        if case == 2 {
            let xml = matched[0].with_extension("plist");
            let value: Value = plist::from_bytes(&std::fs::read(xml).unwrap()).unwrap();
            assert_eq!(value, response);
            assert!(control.cancelled.load(Ordering::SeqCst));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pending_demand_telemetry_cannot_renew_the_single_phase_deadline() {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let (catalog, identity, snapshot) = crate::protocol::ancillary::tests::pending_fixture(140);
    let policy = snapshot
        .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
        .unwrap();
    let control = pending_control(Duration::from_secs(60));
    let started = Instant::now();
    let host = async {
        let session = Session::begin(
            client,
            pending_prepared(),
            &trace.0,
            started + Duration::from_secs(60),
            started + Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut session = EmptyDeltaSession { session };
        session.finish_empty_metadata().await.unwrap();
        assert!(
            session
                .observe_pending_audio(&policy, &control)
                .await
                .is_err()
        );
        assert_eq!(
            Instant::now().duration_since(started),
            Duration::from_secs(35)
        );
    };
    let device = async {
        outgoing(&mut peer).await;
        startup(&mut peer).await;
        outgoing(&mut peer).await;
        peer.write_all(&wire(&pending_ready())).await.unwrap();
        outgoing(&mut peer).await;
        for second in [15, 30] {
            tokio::time::sleep_until(started + Duration::from_secs(second)).await;
            peer.write_all(&wire(&incoming(
                "Progress",
                Some(dict(&[("OverallProgress", 0.5f64.into())])),
            )))
            .await
            .unwrap();
        }
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    };
    tokio::join!(host, device);
    let progress = wire(&incoming(
        "Progress",
        Some(dict(&[("OverallProgress", 0.5f64.into())])),
    ));
    let retained = std::fs::read_dir(&trace.0)
        .unwrap()
        .filter(|entry| {
            let path = entry.as_ref().unwrap().path();
            path.to_string_lossy().ends_with("-received.atc")
                && std::fs::read(path).unwrap() == progress
        })
        .count();
    assert_eq!(retained, 2);
}
