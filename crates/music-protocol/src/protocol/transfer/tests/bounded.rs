use super::*;
use crate::protocol::trace::OutputAllowance;
use std::sync::atomic::AtomicBool;

fn bounded_pair(
    trace: &Path,
    mode: EvidenceMode,
    output: &OutputAllowance,
) -> (Transfer, Idevice, CaptureControl) {
    let (client, peer) = tokio::io::duplex(CHUNK * 2);
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let transfer = Transfer::from_test_duplex_with_output(
        client,
        trace,
        &control,
        MAX_BOUNDED_OPS,
        mode,
        output.clone(),
    )
    .unwrap();
    (
        transfer,
        Idevice::new(Box::new(peer), "offline-peer"),
        control,
    )
}
#[tokio::test]
async fn a_connection_takes_at_most_a_batch_sessions_allowance() {
    let trace = Temporary::new();
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    for (operations, allowed) in [
        (1, true),
        (MAX_SESSION_OPS, true),
        (0, false),
        (MAX_SESSION_OPS + 1, false),
    ] {
        let (client, _peer) = tokio::io::duplex(64);
        let transfer = Transfer::from_test_duplex(client, &trace.0, &control, operations);
        assert_eq!(transfer.is_ok(), allowed, "{operations}");
    }
}

fn trace_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}

#[tokio::test]
async fn detailed_afc_and_later_writer_share_one_monotonic_allowance() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(4096, 8);
    let (mut transfer, mut peer, _) = bounded_pair(&trace.0, EvidenceMode::Detailed, &output);
    let server = tokio::spawn(async move {
        attributes(&mut peer, 7).await;
        assert!(peer.read_raw(1).await.is_err());
    });
    assert!(transfer.stat("Airlock/Media/test").await.unwrap().is_some());
    let usage = output.usage().unwrap();
    assert_eq!(usage.charged_files, 2);
    assert_eq!(usage.charged_bytes, trace_size(&trace.0));
    assert_eq!(usage.reserved_bytes, 0);
    // A later step/writer spends the same remaining allowance. The next AFC
    // intent must fail before any new request reaches the peer.
    output
        .clone()
        .write(
            &trace.0.join("later-phase"),
            &vec![1; (4096 - usage.charged_bytes - 32) as usize],
        )
        .unwrap();
    assert!(transfer.stat("Airlock/Media/next").await.is_err());
    assert!(!transfer.usable);
    assert!(output.usage().unwrap().charged_bytes <= 4096);
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    drop(transfer);
    server.await.unwrap();
}

#[tokio::test]
async fn bounded_afc_retains_malformed_reply_bytes_before_validation() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(4096, 2);
    let (mut transfer, mut peer, _) = bounded_pair(&trace.0, EvidenceMode::Detailed, &output);
    let server = tokio::spawn(async move {
        let request = request(&mut peer, AfcOpcode::GetFileInfo).await;
        let malformed = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: 48,
                header_payload_len: 48,
                packet_num: request.header.packet_num,
                operation: AfcOpcode::FileOpenRes,
            },
            header_payload: 77u64.to_le_bytes().to_vec(),
            payload: Vec::new(),
        };
        let expected = malformed.serialize();
        peer.send_raw(&expected).await.unwrap();
        expected
    });
    assert!(transfer.stat("Airlock/Media/test").await.is_err());
    assert!(!transfer.usable);
    assert_eq!(
        std::fs::read(trace.0.join("afc-0000-reply.bin")).unwrap(),
        server.await.unwrap()
    );
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
}

#[tokio::test]
async fn oversized_afc_reply_retains_bounded_raw_prefix_and_discards_transport() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(2048, 2);
    let (mut transfer, mut peer, _) = bounded_pair(&trace.0, EvidenceMode::Detailed, &output);
    let server = tokio::spawn(async move {
        let request = request(&mut peer, AfcOpcode::GetFileInfo).await;
        reply(
            &mut peer,
            &request,
            AfcOpcode::Data,
            Vec::new(),
            vec![1; 8192],
        )
        .await;
    });
    assert!(transfer.stat("Airlock/Media/test").await.is_err());
    server.await.unwrap();
    let partial = std::fs::read(trace.0.join("afc-0000-reply.bin")).unwrap();
    assert_eq!(partial.len(), 40);
    assert_eq!(
        u64::from_le_bytes(partial[8..16].try_into().unwrap()),
        40 + 8192
    );
    assert!(!transfer.usable);
    assert!(output.usage().unwrap().charged_bytes <= 2048);
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
}

#[tokio::test]
async fn detailed_afc_reserves_reply_capacity_before_sending_request() {
    let legacy = Temporary::new();
    let (mut baseline, mut peer) = pair(&legacy.0, Duration::from_secs(5));
    let server = tokio::spawn(async move {
        attributes(&mut peer, 0).await;
    });
    baseline.stat("Airlock/Media/test").await.unwrap();
    server.await.unwrap();
    let intent_bytes = std::fs::metadata(legacy.0.join("afc-0000-intent.plist"))
        .unwrap()
        .len();
    let trace = Temporary::new();
    let output = OutputAllowance::new(intent_bytes + 39, 3);
    let (mut transfer, mut peer, _) = bounded_pair(&trace.0, EvidenceMode::Detailed, &output);
    assert!(transfer.stat("Airlock/Media/test").await.is_err());
    assert_eq!(output.usage().unwrap().charged_bytes, intent_bytes);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    drop(transfer);
    assert!(peer.read_raw(1).await.is_err());
}

#[tokio::test]
async fn summary_receipts_and_failure_diagnostic_share_protected_output_capacity() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(64 * 1024, 3);
    let (mut transfer, mut peer, _) =
        bounded_pair(&trace.0, EvidenceMode::ReadonlySummary, &output);
    // The refusal below is the summary transport's, not the write lock's.
    transfer.unlock_writes();
    let server = tokio::spawn(async move {
        attributes(&mut peer, 0).await;
        let request = request(&mut peer, AfcOpcode::FileOpen).await;
        reply(
            &mut peer,
            &request,
            AfcOpcode::FileOpenRes,
            42u64.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
        close(&mut peer).await;
        attributes(&mut peer, 0).await;
        assert!(peer.read_raw(1).await.is_err());
    });
    assert_eq!(
        transfer
            .read_hash("Airlock/Media/test", 0, MAX_AUDIO)
            .await
            .unwrap(),
        <[u8; 32]>::from(Sha256::digest([]))
    );
    let usage = output.usage().unwrap();
    assert_eq!(usage.reserved_bytes, CURRENT_METADATA_LIMIT as u64);
    let remainder = 64 * 1024 - usage.charged_bytes - usage.reserved_bytes;
    output
        .write(&trace.0.join("other-writer"), &vec![1; remainder as usize])
        .unwrap();
    assert!(
        transfer
            .write_new("Airlock/Media/new", &mut Cursor::new([1]), 1)
            .await
            .is_err()
    );
    let diagnostic = Value::from_file(trace.0.join("afc-summary-failure.plist")).unwrap();
    assert!(
        diagnostic.as_dictionary().unwrap()["Error"]
            .as_string()
            .unwrap()
            .contains("read-only")
    );
    assert!(trace.0.join("afc-readback-0000.plist").is_file());
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    assert_eq!(output.usage().unwrap().charged_files, 3);
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
    drop(transfer);
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_afc_exchange_keeps_reserved_diagnostic_without_cleanup_io() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(4096, 2);
    let (mut transfer, mut peer, control) = bounded_pair(&trace.0, EvidenceMode::Detailed, &output);
    let server = tokio::spawn(async move {
        request(&mut peer, AfcOpcode::GetFileInfo).await;
        control.cancelled.store(true, Ordering::Relaxed);
        // Keep the transport open so this is cancellation, not EOF.
        assert!(peer.read_raw(1).await.is_err());
    });
    assert!(transfer.stat("Airlock/Media/test").await.is_err());
    assert!(!transfer.usable);
    let error = std::fs::read_to_string(trace.0.join("afc-0000-error.txt")).unwrap();
    assert!(error.contains("cancelled"));
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
    drop(transfer);
    server.await.unwrap();
}

#[tokio::test]
async fn bounded_current_summary_preserves_eight_receipt_cap_and_reserved_failure() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(256 * 1024, 9);
    let (mut transfer, mut peer, _) = bounded_pair(&trace.0, EvidenceMode::CurrentSummary, &output);
    let server = tokio::spawn(async move {
        for _ in 0..CURRENT_READ_RECEIPTS {
            attributes(&mut peer, 0).await;
            let request = request(&mut peer, AfcOpcode::FileOpen).await;
            reply(
                &mut peer,
                &request,
                AfcOpcode::FileOpenRes,
                42u64.to_le_bytes().to_vec(),
                Vec::new(),
            )
            .await;
            close(&mut peer).await;
            attributes(&mut peer, 0).await;
        }
        assert!(peer.read_raw(1).await.is_err());
    });
    for _ in 0..CURRENT_READ_RECEIPTS {
        transfer
            .read_hash("Airlock/Media/test", 0, MAX_AUDIO)
            .await
            .unwrap();
    }
    assert!(
        transfer
            .read_hash("Airlock/Media/test", 0, MAX_AUDIO)
            .await
            .is_err()
    );
    let diagnostic = Value::from_file(trace.0.join("afc-summary-failure.plist")).unwrap();
    assert!(
        diagnostic.as_dictionary().unwrap()["Error"]
            .as_string()
            .unwrap()
            .contains("receipt allowance")
    );
    assert_eq!(output.usage().unwrap().charged_files, 9);
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&trace.0));
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
    for entry in std::fs::read_dir(&trace.0).unwrap() {
        assert!(entry.unwrap().metadata().unwrap().len() <= CURRENT_METADATA_LIMIT as u64);
    }
    drop(transfer);
    server.await.unwrap();
}
