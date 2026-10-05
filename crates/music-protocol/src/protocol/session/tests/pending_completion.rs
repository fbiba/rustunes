use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const DESTINATION: &str = "iTunes_Control/Music/F39/TEST.m4a";
fn pending_prepared() -> Prepared {
    let (request, finished) = references();
    prepare_empty_delta(request, &finished, ID, 140)
        .unwrap()
        .prepared
}
fn pending_ready() -> Value {
    let mut value = ready_message();
    set(
        &mut value,
        &["Params", "DataclassAnchors", "Media"],
        "140".into(),
    );
    value
}
fn control(deadline: Instant) -> crate::protocol::transfer::CaptureControl {
    crate::protocol::transfer::CaptureControl {
        deadline,
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
fn expected_complete() -> Value {
    dict(&[
        ("Session", 2u64.into()),
        ("Command", "FileComplete".into()),
        (
            "Params",
            dict(&[
                ("AssetID", "-1".into()),
                ("AssetPath", DESTINATION.into()),
                ("Dataclass", "Media".into()),
            ]),
        ),
    ])
}
fn expected_error(id: &str) -> Value {
    dict(&[
        ("Session", 2u64.into()),
        ("Command", "FileError".into()),
        (
            "Params",
            dict(&[
                ("AssetID", id.into()),
                ("Dataclass", "Media".into()),
                ("ErrorCode", 16i64.into()),
                ("ErrorIsVisible", false.into()),
            ]),
        ),
    ])
}
async fn handshake(peer: &mut DuplexStream) {
    assert_eq!(
        outgoing(peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
        Some("Ping")
    );
    startup(peer).await;
    assert_eq!(outgoing(peer).await.0, pending_prepared().request);
    peer.write_all(&wire(&pending_ready())).await.unwrap();
    let finished = outgoing(peer).await.0;
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
}
async fn demand(
    client: atc::Atc,
    trace: &Path,
    deadline: Instant,
    control: &crate::protocol::transfer::CaptureControl,
) -> PendingAudioDemand {
    let (catalog, identity, snapshot) =
        crate::protocol::ancillary::tests::pending_wire_fixture(140);
    let policy = snapshot
        .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
        .unwrap();
    let session = Session::begin(
        client,
        pending_prepared(),
        trace,
        deadline,
        deadline.min(Instant::now() + Duration::from_secs(1)),
    )
    .await
    .unwrap();
    let mut session = EmptyDeltaSession { session };
    session.finish_empty_metadata().await.unwrap();
    session
        .observe_pending_audio(&policy, control)
        .await
        .unwrap()
}
fn assert_body_context(context: &PendingBodyContext) {
    assert_eq!(context.own_asset_id(), "-1");
    assert_eq!(context.permanent_path(), DESTINATION);
    assert_eq!(context.media_anchor(), 140);
    assert_ne!(context.ready_sha256(), [0; 32]);
    assert_ne!(context.manifest_sha256(), [0; 32]);
    context.control().check().unwrap();
}

#[tokio::test]
async fn pending_completion_own_five_and_exact_foreign_set_precede_real_terminal() {
    for mask in [1, 5] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        let control = control(deadline);
        let host = async {
            demand(client, &trace.0, deadline, &control)
                .await
                .continue_body_and_complete(DESTINATION, |context| async move {
                    assert_body_context(&context);
                    assert_eq!(context.own_parts(), mask);
                    assert_eq!(context.control().deadline, deadline);
                    assert!(Arc::ptr_eq(
                        &context.control().cancelled,
                        &control.cancelled
                    ));
                    // Synthetic callback tests sequencing only; no real body authority.
                    Ok([11; 32])
                })
                .await
                .unwrap()
        };
        let device = async {
            handshake(&mut peer).await;
            peer.write_all(&wire(&incoming(
                "AssetManifest",
                Some(combined_manifest(&[("-1", mask), ("-888", 2), ("999", 4)])),
            )))
            .await
            .unwrap();
            let mut actual = Vec::new();
            for expected in [
                expected_complete(),
                expected_error("-888"),
                expected_error("999"),
            ] {
                let (value, raw) = outgoing(&mut peer).await;
                assert_eq!(value, expected);
                actual.push(raw);
            }
            // Last chunk/flush has explicit receive checkpoints. Queue a
            // terminal only after local accepted-frame retention; the final
            // retention-to-settlement sequence has no intervening await.
            while std::fs::read_dir(&trace.0)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .to_string_lossy()
                        .ends_with("-accepted.atc")
                })
                .count()
                < 3
            {
                tokio::task::yield_now().await;
            }
            peer.write_all(&wire(&incoming("SyncFinished", None)))
                .await
                .unwrap();
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
            assert!(
                tail.is_empty(),
                "no second metadata finish, own refusal or further reply"
            );
            actual
        };
        let (receipt, actual) = tokio::join!(host, device);
        let evidence = receipt.evidence();
        assert_eq!(evidence.body_verification_sha256, Some([11; 32]));
        assert_eq!(evidence.required_foreign_asset_ids, ["-888", "999"]);
        assert_eq!(
            evidence.settled_foreign_asset_ids,
            evidence.required_foreign_asset_ids
        );
        assert_eq!(evidence.writes.len(), 3);
        for (observed, raw) in evidence.writes.iter().zip(actual) {
            assert_eq!(observed.accepted_bytes, raw.len());
            assert_eq!(observed.accepted_sha256, Some(Sha256::digest(&raw).into()));
            assert!(observed.flushed && observed.intended_trace_complete);
            assert_eq!(
                std::fs::read(
                    trace
                        .0
                        .join(format!("{:03}-accepted.atc", observed.frame_index))
                )
                .unwrap(),
                raw
            );
        }
        let terminal = evidence.terminal.as_ref().unwrap();
        assert!(terminal.started_after_full_settlement);
        assert!(terminal.frame_index > evidence.writes.last().unwrap().frame_index);
    }
}

#[derive(Debug)]
struct Meter {
    inner: DuplexStream,
    written: Arc<AtomicUsize>,
    fail_after: Arc<AtomicUsize>,
    consumed: Arc<AtomicUsize>,
}
impl AsyncRead for Meter {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = output.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, output);
        if matches!(&result, Poll::Ready(Ok(()))) {
            this.consumed
                .fetch_add(output.filled().len() - before, Ordering::SeqCst);
        }
        result
    }
}
impl AsyncWrite for Meter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let current = this.written.load(Ordering::SeqCst);
        let available = this
            .fail_after
            .load(Ordering::SeqCst)
            .saturating_sub(current);
        if available == 0 {
            return Poll::Ready(Err(io::Error::other("injected write failure")));
        }
        let result = Pin::new(&mut this.inner).poll_write(cx, &bytes[..bytes.len().min(available)]);
        if let Poll::Ready(Ok(count)) = &result {
            this.written.fetch_add(*count, Ordering::SeqCst);
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
async fn metered() -> (
    atc::Atc,
    DuplexStream,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (stream, peer) = tokio::io::duplex(64 * 1024);
    let written = Arc::new(AtomicUsize::new(0));
    let fail_after = Arc::new(AtomicUsize::new(usize::MAX));
    let consumed = Arc::new(AtomicUsize::new(0));
    let client = budgeted(
        Idevice::new(
            Box::new(Meter {
                inner: stream,
                written: Arc::clone(&written),
                fail_after: Arc::clone(&fail_after),
                consumed: Arc::clone(&consumed),
            }),
            "offline-only",
        ),
        MAX_BYTES,
    )
    .await
    .unwrap();
    (client, peer, written, fail_after, consumed)
}

#[tokio::test]
async fn pending_completion_mid_refusal_retains_seven_actual_bytes_and_closes() {
    let trace = Temporary::new();
    let (client, mut peer, written, fail_after, _) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(2);
    let control = control(deadline);
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                fail_after.store(
                    written.load(Ordering::SeqCst)
                        + wire(&expected_complete()).len()
                        + wire(&expected_error("-888")).len()
                        + 7,
                    Ordering::SeqCst,
                );
                Ok([11; 32])
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5), ("-888", 2), ("999", 4)])),
        )))
        .await
        .unwrap();
        assert_eq!(outgoing(&mut peer).await.0, expected_complete());
        assert_eq!(outgoing(&mut peer).await.0, expected_error("-888"));
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, wire(&expected_error("999"))[..7]);
        tail
    };
    let (failure, actual) = tokio::join!(host, device);
    let evidence = failure.evidence();
    assert!(evidence.terminal.is_none());
    assert_eq!(evidence.settled_foreign_asset_ids, ["-888"]);
    assert_eq!(evidence.required_foreign_asset_ids, ["-888", "999"]);
    let partial = evidence.writes.last().unwrap();
    assert_eq!(partial.asset_id, "999");
    assert_eq!(partial.accepted_bytes, 7);
    assert!(!partial.flushed);
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-accepted-partial.atc", partial.frame_index))
        )
        .unwrap(),
        actual
    );
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-outgoing.atc", partial.frame_index))
        )
        .unwrap(),
        wire(&expected_error("999"))
    );
}

#[tokio::test]
async fn pending_completion_zero_write_failure_retains_intent_without_invented_bytes() {
    let trace = Temporary::new();
    let (client, mut peer, written, fail_after, _) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(2);
    let control = control(deadline);
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                fail_after.store(written.load(Ordering::SeqCst), Ordering::SeqCst);
                Ok([11; 32])
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5)])),
        )))
        .await
        .unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    let frame = &failure.evidence().writes[0];
    assert_eq!(frame.accepted_bytes, 0);
    assert_eq!(frame.accepted_sha256, None);
    assert!(frame.intended_trace_complete);
    assert!(
        !trace
            .0
            .join(format!("{:03}-accepted.atc", frame.frame_index))
            .exists()
    );
    assert!(
        !trace
            .0
            .join(format!("{:03}-accepted-partial.atc", frame.frame_index))
            .exists()
    );
    assert!(failure.evidence().terminal.is_none());
}

#[tokio::test]
async fn pending_completion_rejects_early_terminal_repeated_unknown_wrong_session_and_body_failure()
{
    let manifest = incoming("AssetManifest", Some(combined_manifest(&[("-1", 5)])));
    let mut wrong = incoming("SyncFinished", None);
    set(&mut wrong, &["Session"], 99u64.into());
    for response in [
        Some(incoming("SyncFinished", None)),
        Some(manifest.clone()),
        Some(incoming("UnknownCompletion", Some(dict(&[])))),
        Some(wrong),
        Some(incoming("SyncFailed", Some(dict(&[])))),
        None,
    ] {
        let trace = Temporary::new();
        let (client, mut peer) = transport(MAX_BYTES).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        let control = control(deadline);
        let called = Arc::new(AtomicUsize::new(0));
        let response_for_host = response.clone();
        let host = async {
            let result = demand(client, &trace.0, deadline, &control)
                .await
                .continue_body_and_complete(DESTINATION, |_| async {
                    called.fetch_add(1, Ordering::SeqCst);
                    if response_for_host.is_none() {
                        bail!("injected body verification failure");
                    }
                    Ok([11; 32])
                })
                .await;
            let failure = result.err().unwrap();
            assert!(failure.evidence().writes.is_empty());
            assert!(failure.evidence().terminal.is_none());
        };
        let device = async {
            handshake(&mut peer).await;
            let mut payload = wire(&manifest);
            if let Some(response) = &response {
                payload.extend_from_slice(&wire(response));
            }
            peer.write_all(&payload).await.unwrap();
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
            assert!(tail.is_empty());
        };
        tokio::join!(host, device);
        assert_eq!(
            called.load(Ordering::SeqCst),
            usize::from(response.is_none())
        );
    }
}

#[tokio::test]
async fn pending_completion_partial_early_terminal_cannot_cross_settlement_boundary() {
    let trace = Temporary::new();
    let (client, mut peer, written, _, consumed) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(2);
    let control = control(deadline);
    let (mut release, mut released) = tokio::io::duplex(1);
    let (mut noticed, mut notice) = tokio::io::duplex(1);
    let body_returned = Arc::new(AtomicBool::new(false));
    let terminal = wire(&incoming("SyncFinished", None));
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                noticed.write_all(&[1]).await?;
                let mut byte = [0];
                released.read_exact(&mut byte).await?;
                body_returned.store(true, Ordering::SeqCst);
                Ok([11; 32])
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        let manifest = wire(&incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5)])),
        ));
        peer.write_all(&manifest).await.unwrap();
        let mut byte = [0];
        notice.read_exact(&mut byte).await.unwrap();
        let before = consumed.load(Ordering::SeqCst);
        let before_outgoing = written.load(Ordering::SeqCst);
        peer.write_all(&terminal[..7]).await.unwrap();
        while consumed.load(Ordering::SeqCst) < before + 7 {
            tokio::task::yield_now().await;
        }
        release.write_all(&[1]).await.unwrap();
        while !body_returned.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            written.load(Ordering::SeqCst),
            before_outgoing,
            "an incomplete early header blocks every following outgoing action"
        );
        peer.write_all(&terminal[7..]).await.unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    assert!(failure.evidence().terminal.is_none());
    assert_eq!(failure.evidence().body_verification_sha256, Some([11; 32]));
    assert!(failure.evidence().writes.is_empty());
    let incoming = failure.evidence().received.last().unwrap();
    assert!(!incoming.started_after_full_settlement);
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-received.atc", incoming.frame_index))
        )
        .unwrap(),
        terminal
    );
}

#[tokio::test]
async fn pending_completion_cancelled_partial_receive_retains_actual_prefix_and_closes_body() {
    let trace = Temporary::new();
    let (client, mut peer, _, _, consumed) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(2);
    let control = control(deadline);
    let terminal = wire(&incoming("SyncFinished", None));
    let body_called = Arc::new(AtomicBool::new(false));
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                body_called.store(true, Ordering::SeqCst);
                std::future::pending::<Result<[u8; 32]>>().await
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        let manifest = wire(&incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5)])),
        ));
        peer.write_all(&manifest).await.unwrap();
        while !body_called.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let before = consumed.load(Ordering::SeqCst);
        peer.write_all(&terminal[..7]).await.unwrap();
        while consumed.load(Ordering::SeqCst) < before + 7 {
            tokio::task::yield_now().await;
        }
        control.cancelled.store(true, Ordering::SeqCst);
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    assert!(failure.evidence().writes.is_empty());
    assert!(failure.evidence().terminal.is_none());
    let partial = failure.evidence().partial_receive.as_ref().unwrap();
    assert_eq!(partial.consumed_bytes, 7);
    assert_eq!(
        partial.consumed_sha256,
        <[u8; 32]>::from(Sha256::digest(&terminal[..7]))
    );
    let matching = std::fs::read_dir(&trace.0)
        .unwrap()
        .filter(|entry| {
            let path = entry.as_ref().unwrap().path();
            path.to_string_lossy().ends_with("-partial.atc")
                && std::fs::read(path).unwrap() == terminal[..7]
        })
        .count();
    assert_eq!(matching, 1);
}

#[tokio::test(start_paused = true)]
async fn pending_completion_cannot_renew_original_channel_deadline_from_late_body_or_telemetry() {
    let trace = Temporary::new();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(40);
    let control = control(started + Duration::from_secs(60));
    let host = async {
        let demand = demand(client, &trace.0, deadline, &control).await;
        tokio::time::sleep_until(started + Duration::from_secs(38)).await;
        let failure = demand
            .continue_body_and_complete(DESTINATION, |context| async move {
                assert_eq!(context.control().deadline, deadline);
                tokio::time::sleep(Duration::from_secs(4)).await;
                Ok([11; 32])
            })
            .await
            .err()
            .unwrap();
        assert_eq!(
            Instant::now().duration_since(started),
            Duration::from_secs(40)
        );
        assert!(failure.evidence().writes.is_empty());
        assert!(failure.evidence().terminal.is_none());
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&incoming(
            "AssetManifest",
            Some(combined_manifest(&[("-1", 5)])),
        )))
        .await
        .unwrap();
        for millis in [38_500, 39_500] {
            tokio::time::sleep_until(started + Duration::from_millis(millis)).await;
            peer.write_all(&wire(&incoming(
                "Progress",
                Some(dict(&[("OverallProgress", 0.5f64.into())])),
            )))
            .await
            .unwrap();
        }
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    tokio::join!(host, device);
}

#[derive(Debug)]
struct ReadyInjection {
    incoming: std::sync::Mutex<std::collections::VecDeque<u8>>,
    accepted: std::sync::Mutex<Vec<u8>>,
    reader_waker: std::sync::Mutex<Option<std::task::Waker>>,
    response: std::sync::Mutex<Option<Vec<u8>>>,
    // 0 = callback/no remaining hook, 1 = first positive write, 2 = first ready flush.
    hook: AtomicUsize,
    consumed_injected: AtomicUsize,
}
impl ReadyInjection {
    fn new() -> Self {
        Self {
            incoming: std::sync::Mutex::new(std::collections::VecDeque::new()),
            accepted: std::sync::Mutex::new(Vec::new()),
            reader_waker: std::sync::Mutex::new(None),
            response: std::sync::Mutex::new(None),
            hook: AtomicUsize::new(0),
            consumed_injected: AtomicUsize::new(0),
        }
    }
    fn inject(&self, bytes: &[u8]) {
        self.incoming.lock().unwrap().extend(bytes.iter().copied());
        if let Some(waker) = self.reader_waker.lock().unwrap().take() {
            waker.wake();
        }
    }
    fn trigger(&self) {
        let bytes = self.response.lock().unwrap().take().unwrap();
        self.inject(&bytes);
    }
}
#[derive(Debug)]
struct ReadyInjectionStream {
    inner: DuplexStream,
    injection: Arc<ReadyInjection>,
}
impl AsyncRead for ReadyInjectionStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut incoming = this.injection.incoming.lock().unwrap();
        if !incoming.is_empty() {
            let count = incoming.len().min(output.remaining());
            let bytes: Vec<_> = incoming.drain(..count).collect();
            output.put_slice(&bytes);
            this.injection
                .consumed_injected
                .fetch_add(count, Ordering::SeqCst);
            return Poll::Ready(Ok(()));
        }
        drop(incoming);
        *this.injection.reader_waker.lock().unwrap() = Some(cx.waker().clone());
        Pin::new(&mut this.inner).poll_read(cx, output)
    }
}
impl AsyncWrite for ReadyInjectionStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let chunk = if this.injection.hook.load(Ordering::SeqCst) == 1 {
            bytes.len().min(7)
        } else {
            bytes.len()
        };
        let result = Pin::new(&mut this.inner).poll_write(cx, &bytes[..chunk]);
        if let Poll::Ready(Ok(count)) = &result {
            this.injection
                .accepted
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..*count]);
            if *count > 0
                && this
                    .injection
                    .hook
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                // Queue input during a Ready positive write; all subsequent
                // writes could also be Ready if owner failed to yield to receive.
                this.injection.trigger();
            }
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        if matches!(&result, Poll::Ready(Ok(())))
            && this
                .injection
                .hook
                .compare_exchange(2, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            this.injection.trigger();
        }
        result
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn pending_completion_ready_callback_chunk_and_flush_injection_stop_next_actions() {
    for mode in 0..3 {
        for command in ["SyncFailed", "SyncFinished"] {
            for partial_header in [false, true] {
                let trace = Temporary::new();
                let (stream, mut peer) = tokio::io::duplex(64 * 1024);
                let injection = Arc::new(ReadyInjection::new());
                let client = budgeted(
                    Idevice::new(
                        Box::new(ReadyInjectionStream {
                            inner: stream,
                            injection: Arc::clone(&injection),
                        }),
                        "offline-only",
                    ),
                    MAX_BYTES,
                )
                .await
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                let control = control(deadline);
                let response = wire(&incoming(
                    command,
                    (command == "SyncFailed").then(|| dict(&[])),
                ));
                let initial = if partial_header {
                    response[..2].to_vec()
                } else {
                    response.clone()
                };
                let expected_bytes = match mode {
                    0 => Vec::new(),
                    1 => wire(&expected_complete())[..7].to_vec(),
                    _ => wire(&expected_complete()),
                };
                let host = async {
                    demand(client, &trace.0, deadline, &control)
                        .await
                        .continue_body_and_complete(DESTINATION, |_| async {
                            // This callback and all hook operations return Ready.
                            // Do not pre-queue input before the owner selector starts.
                            injection.accepted.lock().unwrap().clear();
                            if mode == 0 {
                                injection.inject(&initial);
                            } else {
                                *injection.response.lock().unwrap() = Some(initial.clone());
                                injection.hook.store(mode, Ordering::SeqCst);
                            }
                            Ok([11; 32])
                        })
                        .await
                        .err()
                        .unwrap()
                };
                let device = async {
                    handshake(&mut peer).await;
                    peer.write_all(&wire(&incoming(
                        "AssetManifest",
                        Some(combined_manifest(&[("-1", 5), ("-888", 2), ("999", 4)])),
                    )))
                    .await
                    .unwrap();
                    if partial_header {
                        while injection.consumed_injected.load(Ordering::SeqCst) < 2 {
                            tokio::task::yield_now().await;
                        }
                        // The first two actual header bytes are held. A complete
                        // command is not yet available to reject, so next actions
                        // must remain suspended rather than bypass that prefix.
                        assert_eq!(*injection.accepted.lock().unwrap(), expected_bytes);
                        injection.inject(&response[2..]);
                    }
                    let mut actual = Vec::new();
                    peer.read_to_end(&mut actual).await.unwrap();
                    assert_eq!(
                        actual, expected_bytes,
                        "no next chunk, flush or foreign send after queued {command} in mode {mode}"
                    );
                };
                let (failure, ()) = tokio::join!(host, device);
                let evidence = failure.evidence();
                assert_eq!(evidence.body_verification_sha256, Some([11; 32]));
                assert!(evidence.terminal.is_none());
                assert!(evidence.settled_foreign_asset_ids.is_empty());
                assert_eq!(evidence.required_foreign_asset_ids, ["-888", "999"]);
                assert_eq!(evidence.writes.len(), usize::from(mode != 0));
                let observed = evidence.received.last().unwrap();
                assert!(!observed.started_after_full_settlement);
                assert_eq!(
                    std::fs::read(
                        trace
                            .0
                            .join(format!("{:03}-received.atc", observed.frame_index))
                    )
                    .unwrap(),
                    response
                );
                if let Some(write) = evidence.writes.first() {
                    assert_eq!(write.command, "FileComplete");
                    assert_eq!(write.accepted_bytes, expected_bytes.len());
                    assert_eq!(write.flushed, mode == 2);
                    let suffix = if mode == 1 {
                        "accepted-partial.atc"
                    } else {
                        "accepted.atc"
                    };
                    assert_eq!(
                        std::fs::read(trace.0.join(format!("{:03}-{suffix}", write.frame_index)))
                            .unwrap(),
                        expected_bytes
                    );
                }
            }
        }
    }
}

mod bounded;
