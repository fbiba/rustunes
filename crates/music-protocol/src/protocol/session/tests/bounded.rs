use super::*;
use crate::protocol::{trace::OutputAllowance, transfer::CaptureControl};
use std::sync::{Arc, atomic::AtomicBool};

fn control() -> CaptureControl {
    CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
async fn channel(
    trace: &Path,
    output: &OutputAllowance,
    control: &CaptureControl,
) -> (Channel, DuplexStream) {
    budgeted_channel(trace, output, control, MAX_BYTES).await
}
async fn budgeted_channel(
    trace: &Path,
    output: &OutputAllowance,
    control: &CaptureControl,
    limit: usize,
) -> (Channel, DuplexStream) {
    let (client, peer) = transport(limit).await;
    (
        Channel {
            client: client.with_output(output.clone(), control).unwrap(),
            trace: trace.to_owned(),
            deadline: control.deadline,
            frames: 0,
            last_received_sha256: [0; 32],
        },
        peer,
    )
}
fn size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum()
}
fn xml_size(value: &Value) -> usize {
    let mut xml = Vec::new();
    value.to_writer_xml(&mut xml).unwrap();
    xml.len()
}

#[tokio::test]
async fn outgoing_and_incoming_xml_share_one_output_allowance() {
    for xml_allowance in [128, 256] {
        let trace = Temporary::new();
        let sent = incoming("Ping", None);
        let received = incoming("Pong", None);
        let mut expected_xml = Vec::new();
        received.to_writer_xml(&mut expected_xml).unwrap();
        assert!(expected_xml.len() > xml_allowance);
        let total = wire(&sent).len() + xml_size(&sent) + wire(&received).len() + xml_allowance;
        let output = OutputAllowance::new(total as u64, 4);
        let (mut channel, mut peer) = channel(&trace.0, &output, &control()).await;
        channel.send(&sent).await.unwrap();
        assert_eq!(outgoing(&mut peer).await.0, sent);
        peer.write_all(&wire(&received)).await.unwrap();
        let error = channel.receive().await.unwrap_err();
        assert!(format!("{error:#}").contains("aggregate evidence output allowance exhausted"));
        assert_eq!(
            std::fs::read(trace.0.join("001-received.atc")).unwrap(),
            wire(&received)
        );
        let xml = std::fs::read(trace.0.join("001-received.plist")).unwrap();
        // plist 1.10.1 emits its 164-byte prologue as one write. The sink
        // refuses that whole chunk at 128; 256 permits a real partial prefix.
        assert_eq!(xml.is_empty(), xml_allowance == 128);
        assert!(xml.len() <= xml_allowance && xml.len() < expected_xml.len());
        assert!(expected_xml.starts_with(&xml));
        assert_eq!(output.usage().unwrap().charged_bytes, size(&trace.0));
        assert_eq!(output.usage().unwrap().charged_files, 4);
        assert_eq!(output.usage().unwrap().reserved_bytes, 0);
        assert!(channel.send(&sent).await.is_err());
    }
}

#[tokio::test]
async fn malformed_incoming_plist_retains_exact_raw_and_discards_bounded_connection() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 4);
    let (mut channel, mut peer) = channel(&trace.0, &output, &control()).await;
    let invalid = [5u32.to_le_bytes().as_slice(), b"wrong"].concat();
    peer.write_all(&invalid).await.unwrap();
    assert!(
        channel
            .receive()
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid incoming plist")
    );
    assert_eq!(
        std::fs::read(trace.0.join("000-received.atc")).unwrap(),
        invalid
    );
    assert_eq!(output.usage().unwrap().charged_bytes, invalid.len() as u64);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    assert!(!trace.0.join("000-received.plist").exists());
    assert!(channel.send(&incoming("Ping", None)).await.is_err());
}

/// The byte budget counts every transport byte, both ways: a frame that
/// crosses it fails mid-read and keeps the bytes that arrived.
#[tokio::test]
async fn a_received_frame_past_the_byte_budget_fails_and_keeps_its_prefix() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 4);
    let (mut channel, mut peer) = budgeted_channel(&trace.0, &output, &control(), 6).await;
    let full = wire(&incoming("Pong", None));
    peer.write_all(&full).await.unwrap();
    let error = channel.receive().await.unwrap_err();
    assert!(format!("{error:#}").contains("byte limit"), "{error:#}");
    assert_eq!(
        std::fs::read(trace.0.join("000-partial.atc")).unwrap(),
        full[..6]
    );
}

#[tokio::test]
async fn a_sent_frame_past_the_byte_budget_fails() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 4);
    let (mut channel, _peer) = budgeted_channel(&trace.0, &output, &control(), 3).await;
    let error = channel.send(&incoming("Ping", None)).await.unwrap_err();
    assert!(format!("{error:#}").contains("byte limit"), "{error:#}");
}

#[tokio::test]
async fn frame_larger_than_reserved_raw_capacity_retains_header_without_reading_body() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(8, 1);
    let (mut channel, mut peer) = channel(&trace.0, &output, &control()).await;
    peer.write_all(&100u32.to_le_bytes()).await.unwrap();
    peer.write_all(b"body remains unread").await.unwrap();
    let error = channel.receive().await.unwrap_err();
    assert!(format!("{error:#}").contains("reserved evidence capacity"));
    assert_eq!(channel.client.partial_frame(), 100u32.to_le_bytes());
    assert_eq!(
        std::fs::read(trace.0.join("000-partial.atc")).unwrap(),
        100u32.to_le_bytes()
    );
    assert_eq!(output.usage().unwrap().charged_bytes, 4);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
}

#[tokio::test]
async fn no_raw_retention_capacity_refuses_before_consuming_header() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(3, 1);
    let (mut channel, mut peer) = channel(&trace.0, &output, &control()).await;
    peer.write_all(&100u32.to_le_bytes()).await.unwrap();
    assert!(channel.receive().await.is_err());
    assert!(channel.client.partial_frame().is_empty());
    assert_eq!(output.usage().unwrap().charged_bytes, 0);
    assert_eq!(output.usage().unwrap().charged_files, 0);
    assert_eq!(std::fs::read_dir(&trace.0).unwrap().count(), 0);
}

#[tokio::test(start_paused = true)]
async fn bounded_receive_deadline_retains_reserved_partial_after_control_expires() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 2);
    let mut control = control();
    control.deadline = Instant::now() + Duration::from_millis(20);
    let (mut channel, mut peer) = channel(&trace.0, &output, &control).await;
    let partial = [8u32.to_le_bytes().as_slice(), b"bp"].concat();
    peer.write_all(&partial).await.unwrap();
    assert!(channel.receive().await.is_err());
    assert_eq!(
        std::fs::read(trace.0.join("000-partial.atc")).unwrap(),
        partial
    );
    assert_eq!(output.usage().unwrap().charged_bytes, partial.len() as u64);
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
    assert!(channel.send(&incoming("Ping", None)).await.is_err());
}

#[tokio::test]
async fn bounded_receive_cancellation_retains_partial_with_same_aggregate() {
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 2);
    let control = control();
    let (mut channel, mut peer) = channel(&trace.0, &output, &control).await;
    peer.write_all(&[8, 0]).await.unwrap();
    {
        let mut receive = std::pin::pin!(channel.receive());
        poll_fn(|cx| {
            assert!(receive.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(output.usage().unwrap().reserved_bytes >= 4);
        control.cancelled.store(true, Ordering::Relaxed);
        assert!(receive.await.is_err());
    }
    assert_eq!(
        std::fs::read(trace.0.join("000-partial.atc")).unwrap(),
        [8, 0]
    );
    assert_eq!(output.usage().unwrap().charged_bytes, 2);
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
    assert!(channel.send(&incoming("Ping", None)).await.is_err());
}

#[tokio::test]
async fn dropped_receive_future_keeps_raw_lease_until_transport_drop_retains_partial() {
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };
    let trace = Temporary::new();
    let output = OutputAllowance::new(1024, 2);
    let (mut channel, mut peer) = channel(&trace.0, &output, &control()).await;
    peer.write_all(&[8, 0]).await.unwrap();
    {
        let mut receive = std::pin::pin!(channel.receive());
        poll_fn(|cx| {
            assert!(receive.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    assert_eq!(output.usage().unwrap().reserved_bytes, 1024);
    assert!(output.write(&trace.0.join("other-writer"), b"x").is_err());
    drop(channel);
    assert_eq!(
        std::fs::read(trace.0.join("000-partial.atc")).unwrap(),
        [8, 0]
    );
    assert_eq!(output.usage().unwrap().charged_bytes, 2);
    assert_eq!(output.usage().unwrap().reserved_bytes, 0);
}

#[tokio::test]
async fn afc_and_atc_duplexes_cannot_reset_shared_file_allowance_between_phases() {
    use crate::protocol::transfer::{EvidenceMode, MAX_BOUNDED_OPS, Transfer};
    use idevice::afc::{
        MAGIC,
        opcode::AfcOpcode,
        packet::{AfcPacket, AfcPacketHeader},
    };
    let afc_trace = Temporary::new();
    let atc_trace = Temporary::new();
    let output = OutputAllowance::new(8192, 4);
    let control = control();
    let (endpoint, peer) = tokio::io::duplex(4096);
    let mut transfer = Transfer::from_test_duplex_with_output(
        endpoint,
        &afc_trace.0,
        &control,
        MAX_BOUNDED_OPS,
        EvidenceMode::Detailed,
        output.clone(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let mut peer = Idevice::new(Box::new(peer), "offline-afc-peer");
        let request = AfcPacket::read(&mut peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::GetFileInfo);
        let payload = b"st_size\x000\x00".to_vec();
        let response = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: 40 + payload.len() as u64,
                header_payload_len: 40,
                packet_num: request.header.packet_num,
                operation: AfcOpcode::Data,
            },
            header_payload: Vec::new(),
            payload,
        };
        peer.send_raw(&response.serialize()).await.unwrap();
        assert!(peer.read_raw(1).await.is_err());
    });
    transfer.stat("Airlock/Media/test").await.unwrap();
    let (mut channel, mut peer) = channel(&atc_trace.0, &output, &control).await;
    let sent = incoming("Ping", None);
    channel.send(&sent).await.unwrap();
    assert_eq!(outgoing(&mut peer).await.0, sent);
    assert_eq!(output.usage().unwrap().charged_files, 4);
    assert_eq!(
        output.usage().unwrap().charged_bytes,
        size(&afc_trace.0) + size(&atc_trace.0)
    );
    assert!(transfer.stat("Airlock/Media/next").await.is_err());
    drop(transfer);
    server.await.unwrap();
}

#[tokio::test]
async fn bounded_metadata_session_keeps_handshake_finish_and_terminal_order() {
    let trace = Temporary::new();
    let output = OutputAllowance::new(64 * 1024, 32);
    let control = control();
    let (client, mut peer) = transport(MAX_BYTES).await;
    let client = client.with_output(output.clone(), &control).unwrap();
    let host = async {
        let mut session = Session::begin(
            client,
            prepared(),
            &trace.0,
            control.deadline,
            control.deadline,
        )
        .await
        .unwrap();
        session.finish_metadata().await.unwrap();
        session
            .wait_metadata_only(&metadata_policy())
            .await
            .unwrap();
        assert!(
            session
                .wait_metadata_only(&metadata_policy())
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
        assert_eq!(
            outgoing(&mut peer).await.0.as_dictionary().unwrap()["Command"].as_string(),
            Some("RequestingSync")
        );
        peer.write_all(&wire(&ready_message())).await.unwrap();
        assert_eq!(
            outgoing(&mut peer).await.0,
            prepared().finished_message().unwrap()
        );
        peer.write_all(&wire(&incoming("SyncFinished", None)))
            .await
            .unwrap();
        let mut extra = Vec::new();
        peer.read_to_end(&mut extra).await.unwrap();
        assert!(extra.is_empty());
    };
    tokio::join!(host, device);
    let usage = output.usage().unwrap();
    assert_eq!(usage.charged_bytes, size(&trace.0));
    assert_eq!(usage.charged_files, 16);
    assert_eq!(usage.reserved_bytes, 0);
}
