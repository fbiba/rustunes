use super::*;
use crate::protocol::transfer::{CaptureControl, MAX_BOUNDED_OPS, METADATA, SIGNATURE, Transfer};
use idevice::afc::{
    MAGIC,
    opcode::AfcOpcode,
    packet::{AfcPacket, AfcPacketHeader},
};
use std::sync::{Arc, atomic::AtomicBool};

const OPERATION: &[u8] = b"exact serialized metadata operation";

struct Started {
    operation: Vec<u8>,
    signature: [u8; 21],
}
impl StartedOperation for Started {
    fn operation(&self) -> &[u8] {
        &self.operation
    }
    fn signature(&self) -> &[u8; 21] {
        &self.signature
    }
}

/// The Ready reply of a device whose Grappa material is `fill`: another fill is
/// another session key.
fn ready_with(fill: u8) -> Value {
    let mut grappa = vec![fill; 83];
    grappa[..2].copy_from_slice(&[1, 1]);
    grappa[18] = 0x40;
    let mut ready = ready_message();
    set(
        &mut ready,
        &["Params", "DeviceInfo", "Grappa"],
        Value::Data(grappa),
    );
    ready
}

/// A session in Ready whose whole handshake was queued before it began.
async fn ready_session(trace: &Path, fill: u8) -> (Session, DuplexStream) {
    let (client, mut peer) = transport(MAX_BYTES).await;
    startup(&mut peer).await;
    peer.write_all(&wire(&ready_with(fill))).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let session = Session::begin(client, prepared(), trace, deadline, deadline)
        .await
        .unwrap();
    (session, peer)
}

fn started(session: &Session) -> Started {
    Started {
        operation: OPERATION.to_vec(),
        signature: session.sign_metadata(OPERATION).unwrap(),
    }
}

/// A locked AFC connection and the phone's end of it.
fn locked_transfer(trace: &Path) -> (Transfer, Idevice) {
    let control = CaptureControl {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (endpoint, peer) = tokio::io::duplex(64 * 1024);
    (
        Transfer::from_test_duplex(endpoint, trace, &control, MAX_BOUNDED_OPS).unwrap(),
        Idevice::new(Box::new(peer), "offline-afc-peer"),
    )
}

async fn request(peer: &mut Idevice, expected: AfcOpcode) -> AfcPacket {
    let packet = AfcPacket::read(peer).await.unwrap();
    assert_eq!(packet.header.operation, expected);
    packet
}

async fn reply(
    peer: &mut Idevice,
    request: &AfcPacket,
    operation: AfcOpcode,
    header: Vec<u8>,
    payload: Vec<u8>,
) {
    let header_payload_len = 40 + header.len() as u64;
    let packet = AfcPacket {
        header: AfcPacketHeader {
            magic: MAGIC,
            entire_len: header_payload_len + payload.len() as u64,
            header_payload_len,
            packet_num: request.header.packet_num,
            operation,
        },
        header_payload: header,
        payload,
    };
    peer.send_raw(&packet.serialize()).await.unwrap();
}

async fn status(peer: &mut Idevice, request: &AfcPacket, code: u64) {
    reply(
        peer,
        request,
        AfcOpcode::Status,
        code.to_le_bytes().to_vec(),
        Vec::new(),
    )
    .await;
}

async fn attributes(peer: &mut Idevice, kind: &str, size: usize) {
    let packet = request(peer, AfcOpcode::GetFileInfo).await;
    let bytes = format!("st_ifmt\0{kind}\0st_size\0{size}\0st_mtime\0123\0").into_bytes();
    reply(peer, &packet, AfcOpcode::Data, Vec::new(), bytes).await;
}

/// The phone's side of `Transfer::write_new` for a file small enough to be one
/// chunk. Returns the path it was opened at and the bytes it was sent.
async fn serve_new_file(peer: &mut Idevice) -> (String, Vec<u8>) {
    let absent = request(peer, AfcOpcode::GetFileInfo).await;
    status(peer, &absent, 8).await;
    let open = request(peer, AfcOpcode::FileOpen).await;
    assert_eq!(&open.header_payload[..8], &2_u64.to_le_bytes());
    let path = String::from_utf8(open.header_payload[8..].to_vec()).unwrap();
    reply(
        peer,
        &open,
        AfcOpcode::FileOpenRes,
        42_u64.to_le_bytes().to_vec(),
        Vec::new(),
    )
    .await;
    attributes(peer, "S_IFREG", 0).await;
    let write = request(peer, AfcOpcode::Write).await;
    status(peer, &write, 0).await;
    let size = request(peer, AfcOpcode::FileSetSize).await;
    assert_eq!(
        size.header_payload,
        [
            42_u64.to_le_bytes(),
            (write.payload.len() as u64).to_le_bytes()
        ]
        .concat()
    );
    status(peer, &size, 0).await;
    let close = request(peer, AfcOpcode::FileClose).await;
    status(peer, &close, 0).await;
    attributes(peer, "S_IFREG", write.payload.len()).await;
    (path, write.payload)
}

/// The phone's side of `ensure_directory` on a directory that exists.
async fn serve_existing_directory(peer: &mut Idevice) {
    attributes(peer, "S_IFDIR", 64).await;
    attributes(peer, "S_IFDIR", 64).await;
}

const REFUSED: &str = "no Started permit";

#[tokio::test]
async fn a_fresh_transfer_refuses_every_write_and_sends_no_packet() {
    let afc_trace = Temporary::new();
    let (mut afc, mut peer) = locked_transfer(&afc_trace.0);
    for (method, error) in [
        (
            "write_new",
            afc.write_new("Airlock/Media/1", &mut std::io::Cursor::new([1]), 1)
                .await
                .unwrap_err(),
        ),
        (
            "write_audio_new",
            afc.write_audio_new("Airlock/Media/1", &mut std::io::Cursor::new([1]), 1)
                .await
                .unwrap_err(),
        ),
        (
            "rename_absent",
            afc.rename_absent("Airlock/Media/1.m4a", "Airlock/Media/1", 1)
                .await
                .unwrap_err(),
        ),
        (
            "ensure_directory",
            afc.ensure_directory("Airlock").await.unwrap_err(),
        ),
    ] {
        let text = format!("{error:#}");
        assert!(text.contains(method) && text.contains(REFUSED), "{text}");
    }
    drop(afc);
    assert!(peer.read_raw(1).await.is_err());
    assert!(std::fs::read_dir(&afc_trace.0).unwrap().next().is_none());
}

#[tokio::test]
async fn stage_metadata_refuses_a_signature_this_session_did_not_make() {
    let (atc_trace, other_trace, afc_trace) =
        (Temporary::new(), Temporary::new(), Temporary::new());
    let (mut session, _atc) = ready_session(&atc_trace.0, 7).await;
    let (other, _other_atc) = ready_session(&other_trace.0, 9).await;
    let own = started(&session);
    let foreign = started(&other);
    assert_ne!(own.signature, foreign.signature);
    let mut flipped_signature = started(&session);
    flipped_signature.signature[5] ^= 1;
    let mut flipped_envelope = started(&session);
    flipped_envelope.signature[0] ^= 1;
    let mut flipped_operation = started(&session);
    flipped_operation.operation[3] ^= 1;
    let (mut afc, mut peer) = locked_transfer(&afc_trace.0);
    for refused in [
        foreign,
        flipped_signature,
        flipped_envelope,
        flipped_operation,
    ] {
        let error = session
            .stage_metadata(&mut afc, &refused)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("Started signature was not made by this session"),
            "{error:#}"
        );
        // The refusal left the transfer locked and the session in Ready.
        let locked = afc.ensure_directory("Airlock").await.unwrap_err();
        assert!(format!("{locked:#}").contains(REFUSED));
        assert!(session.phase == Phase::Ready);
    }
    // The same record, signed by this session, is the one that passes.
    assert!(session.permit_writes(&mut afc, &own).is_ok());
    drop(afc);
    assert!(peer.read_raw(1).await.is_err());
    assert!(std::fs::read_dir(&afc_trace.0).unwrap().next().is_none());
}

#[tokio::test]
async fn stage_metadata_outside_ready_refuses_before_any_packet() {
    let (atc_trace, afc_trace) = (Temporary::new(), Temporary::new());
    let (mut session, _atc) = ready_session(&atc_trace.0, 7).await;
    let own = started(&session);
    session.finish_metadata().await.unwrap();
    let (mut afc, mut peer) = locked_transfer(&afc_trace.0);
    let error = session.stage_metadata(&mut afc, &own).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("outside Ready state"),
        "{error:#}"
    );
    let locked = afc.ensure_directory("Airlock").await.unwrap_err();
    assert!(format!("{locked:#}").contains(REFUSED));
    drop(afc);
    assert!(peer.read_raw(1).await.is_err());
    assert!(std::fs::read_dir(&afc_trace.0).unwrap().next().is_none());
}

#[tokio::test]
async fn stage_metadata_writes_metadata_then_signature_and_unlocks_the_transfer() {
    let (atc_trace, afc_trace) = (Temporary::new(), Temporary::new());
    let (mut session, _atc) = ready_session(&atc_trace.0, 7).await;
    let own = started(&session);
    let (mut afc, mut peer) = locked_transfer(&afc_trace.0);
    let server = tokio::spawn(async move {
        let metadata = serve_new_file(&mut peer).await;
        let signature = serve_new_file(&mut peer).await;
        // Writable now: a directory write goes through to the phone.
        serve_existing_directory(&mut peer).await;
        assert!(peer.read_raw(1).await.is_err());
        [metadata, signature]
    });
    session.stage_metadata(&mut afc, &own).await.unwrap();
    // Staging leaves the phase to the caller, who checks its control first.
    assert!(session.phase == Phase::Ready);
    afc.ensure_directory("Airlock").await.unwrap();
    drop(afc);
    assert_eq!(
        server.await.unwrap(),
        [
            (METADATA.to_owned(), OPERATION.to_vec()),
            (SIGNATURE.to_owned(), own.signature.to_vec()),
        ]
    );
    session.finish_metadata().await.unwrap();
}

#[tokio::test]
async fn permit_writes_unlocks_a_second_transfer_for_this_sessions_permit_and_refuses_a_foreign_one()
 {
    let (atc_trace, other_trace) = (Temporary::new(), Temporary::new());
    let (mut session, _atc) = ready_session(&atc_trace.0, 7).await;
    let (other, _other_atc) = ready_session(&other_trace.0, 9).await;
    let own = started(&session);
    let foreign = started(&other);

    let foreign_trace = Temporary::new();
    let (mut refused, mut refused_peer) = locked_transfer(&foreign_trace.0);
    let error = session.permit_writes(&mut refused, &foreign).unwrap_err();
    assert!(
        format!("{error:#}").contains("Started signature was not made by this session"),
        "{error:#}"
    );
    let locked = refused.ensure_directory("Airlock").await.unwrap_err();
    assert!(format!("{locked:#}").contains(REFUSED));
    drop(refused);
    assert!(refused_peer.read_raw(1).await.is_err());

    // Ready, and after the metadata is finished: any phase may unlock.
    for finished in [false, true] {
        if finished {
            session.finish_metadata().await.unwrap();
        }
        let upload_trace = Temporary::new();
        let (mut upload, mut peer) = locked_transfer(&upload_trace.0);
        let server = tokio::spawn(async move {
            serve_existing_directory(&mut peer).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        session.permit_writes(&mut upload, &own).unwrap();
        upload.ensure_directory("Airlock").await.unwrap();
        drop(upload);
        server.await.unwrap();
    }
}
