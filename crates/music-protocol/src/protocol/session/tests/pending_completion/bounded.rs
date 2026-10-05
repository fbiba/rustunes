use super::*;
use crate::protocol::trace::OutputAllowance;

const LIMIT: u64 = 64 * 1024;
fn account(output: &OutputAllowance, trace: &Path) {
    let entries: Vec<_> = std::fs::read_dir(trace)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    let bytes: u64 = entries.iter().map(|e| e.metadata().unwrap().len()).sum();
    let usage = output.usage().unwrap();
    assert_eq!(usage.charged_bytes, bytes);
    assert_eq!(usage.reserved_bytes, 0);
    assert!(usage.charged_files >= entries.len() as u64);
    assert!(usage.charged_bytes <= LIMIT);
}
async fn wait_flag(flag: &AtomicBool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn wait_accepted(trace: &Path, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let actual = std::fs::read_dir(trace)
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .to_string_lossy()
                        .ends_with("-accepted.atc")
                })
                .count();
            if actual >= count {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn spend_free(output: &OutputAllowance, path: &Path, leave: u64) {
    let usage = output.usage().unwrap();
    let free = LIMIT - usage.charged_bytes - usage.reserved_bytes;
    assert!(free >= leave);
    output
        .write(path, &vec![0x5a; (free - leave) as usize])
        .unwrap();
}
fn manifest(mask: u64, foreign: bool) -> Value {
    let records = if foreign {
        vec![("-1", mask), ("-888", 2), ("999", 4)]
    } else {
        vec![("-1", mask)]
    };
    incoming("AssetManifest", Some(combined_manifest(&records)))
}

#[tokio::test]
async fn bounded_pending_genuine_demand_carries_one_allowance_through_full_settlement() {
    for mask in [1, 5] {
        let trace = Temporary::new();
        let output = OutputAllowance::new(LIMIT, 64);
        let (client, mut peer) = transport(MAX_BYTES).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let control = control(deadline);
        let client = client.with_output(output.clone(), &control).unwrap();
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
                    // Observational callback only, as in the legacy wire fixture.
                    Ok([11; 32])
                })
                .await
                .unwrap()
        };
        let device = async {
            handshake(&mut peer).await;
            peer.write_all(&wire(&manifest(mask, true))).await.unwrap();
            for expected in [
                expected_complete(),
                expected_error("-888"),
                expected_error("999"),
            ] {
                assert_eq!(outgoing(&mut peer).await.0, expected);
            }
            wait_accepted(&trace.0, 3).await;
            peer.write_all(&wire(&incoming("SyncFinished", None)))
                .await
                .unwrap();
            let mut extra = Vec::new();
            peer.read_to_end(&mut extra).await.unwrap();
            assert!(extra.is_empty());
        };
        let (receipt, ()) = tokio::join!(host, device);
        assert_eq!(
            receipt.evidence().settled_foreign_asset_ids,
            ["-888", "999"]
        );
        assert!(
            receipt
                .evidence()
                .terminal
                .as_ref()
                .unwrap()
                .started_after_full_settlement
        );
        assert_eq!(output.usage().unwrap().charged_files, 27);
        account(&output, &trace.0);
    }
}

#[tokio::test]
async fn bounded_pending_accepted_prefix_survives_mid_refusal_failure() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer, written, fail_after, _) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
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
        peer.write_all(&wire(&manifest(5, true))).await.unwrap();
        assert_eq!(outgoing(&mut peer).await.0, expected_complete());
        assert_eq!(outgoing(&mut peer).await.0, expected_error("-888"));
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, wire(&expected_error("999"))[..7]);
        tail
    };
    let (failure, tail) = tokio::join!(host, device);
    assert!(failure.evidence().terminal.is_none());
    let observed = failure.evidence().writes.last().unwrap();
    assert_eq!(observed.accepted_bytes, 7);
    assert!(!observed.flushed);
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-accepted-partial.atc", observed.frame_index))
        )
        .unwrap(),
        tail
    );
    assert_eq!(failure.evidence().settled_foreign_asset_ids, ["-888"]);
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_cancellation_retains_partial_after_other_writer_spends_free_capacity() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer, _, _, consumed) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
    let body_called = Arc::new(AtomicBool::new(false));
    let terminal = wire(&incoming("SyncFinished", None));
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
        peer.write_all(&wire(&manifest(5, false))).await.unwrap();
        wait_flag(&body_called).await;
        let before = consumed.load(Ordering::SeqCst);
        peer.write_all(&terminal[..7]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while consumed.load(Ordering::SeqCst) < before + 7 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            output.usage().unwrap().reserved_bytes,
            terminal.len() as u64
        );
        spend_free(&output, &trace.0.join("concurrent-other-writer"), 0);
        control.cancelled.store(true, Ordering::SeqCst);
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    let partial = failure.evidence().partial_receive.as_ref().unwrap();
    assert_eq!(partial.consumed_bytes, 7);
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-partial.atc", partial.frame_index))
        )
        .unwrap(),
        terminal[..7]
    );
    assert!(failure.evidence().writes.is_empty());
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_refuses_unreserved_accepted_output_before_first_completion_byte() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer) = transport(MAX_BYTES).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                spend_free(
                    &output,
                    &trace.0.join("body-phase-output"),
                    2 * wire(&expected_complete()).len() as u64 - 1,
                );
                Ok([11; 32])
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&manifest(5, false))).await.unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    assert_eq!(failure.evidence().body_verification_sha256, Some([11; 32]));
    let observed = &failure.evidence().writes[0];
    assert_eq!(observed.accepted_bytes, 0);
    assert!(!observed.intended_trace_complete && !observed.flushed);
    assert!(failure.evidence().terminal.is_none());
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_failed_growth_retains_header_without_consuming_body() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer, _, _, consumed) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
    let body_called = Arc::new(AtomicBool::new(false));
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async {
                spend_free(&output, &trace.0.join("body-phase-output"), 0);
                body_called.store(true, Ordering::SeqCst);
                std::future::pending::<Result<[u8; 32]>>().await
            })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&manifest(5, false))).await.unwrap();
        wait_flag(&body_called).await;
        let before = consumed.load(Ordering::SeqCst);
        peer.write_all(&100u32.to_le_bytes()).await.unwrap();
        peer.write_all(b"not consumed").await.unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
        assert_eq!(consumed.load(Ordering::SeqCst) - before, 4);
    };
    let (failure, ()) = tokio::join!(host, device);
    assert!(format!("{}", failure).contains("cannot grow input retention"));
    let partial = failure.evidence().partial_receive.as_ref().unwrap();
    assert_eq!(partial.consumed_bytes, 4);
    assert_eq!(
        std::fs::read(
            trace
                .0
                .join(format!("{:03}-partial.atc", partial.frame_index))
        )
        .unwrap(),
        100u32.to_le_bytes()
    );
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_file_count_cannot_reset_after_handshake_or_own_completion() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 21);
    let (client, mut peer) = transport(MAX_BYTES).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
    let host = async {
        demand(client, &trace.0, deadline, &control)
            .await
            .continue_body_and_complete(DESTINATION, |_| async { Ok([11; 32]) })
            .await
            .err()
            .unwrap()
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&manifest(5, true))).await.unwrap();
        assert_eq!(outgoing(&mut peer).await.0, expected_complete());
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    let (failure, ()) = tokio::join!(host, device);
    assert_eq!(output.usage().unwrap().charged_files, 21);
    assert_eq!(
        failure.evidence().writes[0].accepted_bytes,
        wire(&expected_complete()).len()
    );
    assert_eq!(failure.evidence().writes[1].accepted_bytes, 0);
    assert!(failure.evidence().settled_foreign_asset_ids.is_empty());
    assert!(failure.evidence().terminal.is_none());
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_bad_plist_and_xml_expansion_keep_raw_evidence_without_reply() {
    for xml_allowance in [None, Some(128), Some(256)] {
        let trace = Temporary::new();
        let output = OutputAllowance::new(LIMIT, 64);
        let (client, mut peer) = transport(MAX_BYTES).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let control = control(deadline);
        let client = client.with_output(output.clone(), &control).unwrap();
        let (frame, expected_xml) = if let Some(xml_allowance) = xml_allowance {
            let message = incoming("InstalledAssets", Some(Value::String("<&>".repeat(512))));
            let mut xml = Vec::new();
            message.to_writer_xml(&mut xml).unwrap();
            assert!(xml.len() > xml_allowance);
            (wire(&message), Some(xml))
        } else {
            ([5u32.to_le_bytes().as_slice(), b"wrong"].concat(), None)
        };
        let body_called = Arc::new(AtomicBool::new(false));
        let host = async {
            demand(client, &trace.0, deadline, &control)
                .await
                .continue_body_and_complete(DESTINATION, |_| async {
                    if let Some(xml_allowance) = xml_allowance {
                        spend_free(
                            &output,
                            &trace.0.join("body-phase-output"),
                            frame.len() as u64 - 4 + xml_allowance as u64,
                        );
                    }
                    body_called.store(true, Ordering::SeqCst);
                    std::future::pending::<Result<[u8; 32]>>().await
                })
                .await
                .err()
                .unwrap()
        };
        let device = async {
            handshake(&mut peer).await;
            peer.write_all(&wire(&manifest(5, false))).await.unwrap();
            wait_flag(&body_called).await;
            peer.write_all(&frame).await.unwrap();
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
            assert!(tail.is_empty());
        };
        let (failure, ()) = tokio::join!(host, device);
        let observed = failure.evidence().received.last().unwrap();
        assert_eq!(
            std::fs::read(
                trace
                    .0
                    .join(format!("{:03}-received.atc", observed.frame_index))
            )
            .unwrap(),
            frame
        );
        assert!(failure.evidence().writes.is_empty());
        assert!(failure.evidence().terminal.is_none());
        let xml_path = trace
            .0
            .join(format!("{:03}-received.plist", observed.frame_index));
        if let Some(xml_allowance) = xml_allowance {
            assert!(format!("{failure}").contains("aggregate evidence output allowance exhausted"));
            let partial = std::fs::read(xml_path).unwrap();
            let expected_xml = expected_xml.unwrap();
            // The first 164-byte serializer chunk cannot fit in 128 bytes;
            // 256 admits a prefix, which remains retained after refusal.
            assert_eq!(partial.is_empty(), xml_allowance == 128);
            assert!(partial.len() <= xml_allowance && partial.len() < expected_xml.len());
            assert!(expected_xml.starts_with(&partial));
        } else {
            assert!(format!("{failure}").contains("invalid pending incoming plist"));
            assert!(!xml_path.exists());
        }
        account(&output, &trace.0);
    }
}

#[tokio::test]
async fn dropped_bounded_pending_continuation_retains_transport_owned_partial_lease() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer, _, _, consumed) = metered().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
    let body_called = Arc::new(AtomicBool::new(false));
    let may_drop = Arc::new(AtomicBool::new(false));
    let terminal = wire(&incoming("SyncFinished", None));
    let host = async {
        let demand = demand(client, &trace.0, deadline, &control).await;
        {
            let mut completion =
                std::pin::pin!(demand.continue_body_and_complete(DESTINATION, |_| async {
                    body_called.store(true, Ordering::SeqCst);
                    std::future::pending::<Result<[u8; 32]>>().await
                }));
            tokio::select! {
                _ = &mut completion => panic!("continuation unexpectedly completed"),
                _ = wait_flag(&may_drop) => {}
            }
        }
    };
    let device = async {
        handshake(&mut peer).await;
        peer.write_all(&wire(&manifest(5, false))).await.unwrap();
        wait_flag(&body_called).await;
        let before = consumed.load(Ordering::SeqCst);
        peer.write_all(&terminal[..7]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while consumed.load(Ordering::SeqCst) < before + 7 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        may_drop.store(true, Ordering::SeqCst);
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    };
    tokio::join!(host, device);
    let matches = std::fs::read_dir(&trace.0)
        .unwrap()
        .filter(|entry| {
            let path = entry.as_ref().unwrap().path();
            path.to_string_lossy().ends_with("-partial.atc")
                && std::fs::read(path).unwrap() == terminal[..7]
        })
        .count();
    assert_eq!(matches, 1);
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_empty_delta_refuses_replaced_control_before_manifest_consumption() {
    for different_deadline in [false, true] {
        let trace = Temporary::new();
        let output = OutputAllowance::new(LIMIT, 64);
        let (client, mut peer) = transport(MAX_BYTES).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let original = control(deadline);
        let client = client.with_output(output.clone(), &original).unwrap();
        let replacement = if different_deadline {
            crate::protocol::transfer::CaptureControl {
                deadline: deadline + Duration::from_secs(1),
                cancelled: original.cancelled.clone(),
            }
        } else {
            control(deadline)
        };
        let host = async {
            let (catalog, identity, snapshot) =
                crate::protocol::ancillary::tests::pending_wire_fixture(140);
            let policy = snapshot
                .pending_audio_with_disabled_genius(&catalog, &identity, "-1")
                .unwrap();
            let session = Session::begin(client, pending_prepared(), &trace.0, deadline, deadline)
                .await
                .unwrap();
            let mut session = EmptyDeltaSession { session };
            session.finish_empty_metadata().await.unwrap();
            let error = session
                .observe_pending_audio(&policy, &replacement)
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("original control"));
        };
        let device = async {
            handshake(&mut peer).await;
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
            assert!(tail.is_empty());
        };
        tokio::join!(host, device);
        assert_eq!(output.usage().unwrap().charged_files, 14);
        account(&output, &trace.0);
    }
}

#[tokio::test(start_paused = true)]
async fn bounded_pending_telemetry_and_body_cannot_renew_original_deadline() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(LIMIT, 64);
    let (client, mut peer) = transport(MAX_BYTES).await;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(40);
    let control = control(deadline);
    let client = client.with_output(output.clone(), &control).unwrap();
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
        peer.write_all(&wire(&manifest(5, false))).await.unwrap();
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
    account(&output, &trace.0);
}

#[tokio::test]
async fn bounded_pending_protocol_length_refusal_precedes_reservation_growth() {
    for advertised in [0, u32::MAX] {
        let trace = Temporary::new();
        let output = OutputAllowance::new(LIMIT, 64);
        let (client, mut peer, _, _, consumed) = metered().await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let control = control(deadline);
        let client = client.with_output(output.clone(), &control).unwrap();
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
            peer.write_all(&wire(&manifest(5, false))).await.unwrap();
            wait_flag(&body_called).await;
            let before = consumed.load(Ordering::SeqCst);
            peer.write_all(&advertised.to_le_bytes()).await.unwrap();
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
            assert!(tail.is_empty());
            assert_eq!(consumed.load(Ordering::SeqCst) - before, 4);
        };
        let (failure, ()) = tokio::join!(host, device);
        let error = format!("{failure}");
        assert!(error.contains("pending incoming frame bound"));
        assert!(!error.contains("cannot grow input retention"));
        let partial = failure.evidence().partial_receive.as_ref().unwrap();
        assert_eq!(partial.consumed_bytes, 4);
        assert_eq!(
            std::fs::read(
                trace
                    .0
                    .join(format!("{:03}-partial.atc", partial.frame_index))
            )
            .unwrap(),
            advertised.to_le_bytes()
        );
        assert!(failure.evidence().writes.is_empty());
        account(&output, &trace.0);
    }
}
