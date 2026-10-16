use super::*;
use crate::engine::state::journal::test_support;
use std::sync::atomic::AtomicBool;

/// A traced frame as the session writes it: length prefix, binary plist.
fn traced(send: &Path, name: &str, command: &str) {
    let mut root = plist::Dictionary::new();
    root.insert("Command".into(), command.into());
    root.insert("Session".into(), 2_u64.into());
    let mut body = Vec::new();
    plist::to_writer_binary(&mut body, &plist::Value::Dictionary(root)).unwrap();
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend(body);
    fs::write(send.join(name), frame).unwrap();
}

fn send_dir(temp: &test_support::Temp) -> std::path::PathBuf {
    let send = temp.0.join("send");
    fs::create_dir(&send).unwrap();
    send
}

#[test]
fn only_a_traced_finish_counts_as_sent() {
    let temp = test_support::Temp::new();
    let send = send_dir(&temp);
    traced(&send, "000-outgoing.atc", "RequestingSync");
    traced(&send, "001-received.atc", "FinishedSyncingMetadata");
    traced(&send, "002-received.atc", "ReadyForSync");
    assert!(!finish_traced(&send).unwrap());
    traced(&send, "003-outgoing.atc", "FinishedSyncingMetadata");
    assert!(finish_traced(&send).unwrap());
}

#[test]
fn a_trace_without_its_request_or_with_a_cut_frame_proves_nothing() {
    let temp = test_support::Temp::new();
    let send = send_dir(&temp);
    traced(&send, "001-received.atc", "ReadyForSync");
    let error = finish_traced(&send).unwrap_err();
    assert!(error.to_string().contains("no RequestingSync"), "{error:#}");
    traced(&send, "000-outgoing.atc", "RequestingSync");
    fs::write(send.join("002-outgoing.atc"), [9, 0, 0, 0, b'b']).unwrap();
    let error = finish_traced(&send).unwrap_err();
    assert!(error.to_string().contains("does not parse"), "{error:#}");
    let error = finish_traced(&temp.0.join("missing")).unwrap_err();
    assert!(
        error.to_string().contains("reading the send trace"),
        "{error:#}"
    );
}

#[tokio::test]
async fn refusals_come_before_any_connection() {
    let temp = test_support::Temp::new();
    let send = send_dir(&temp);
    traced(&send, "000-outgoing.atc", "RequestingSync");
    let identity = test_support::identity();
    let mut preferences = vec![0; 1232];
    preferences[..4].copy_from_slice(b"frpd");
    preferences[4..6].copy_from_slice(&1u16.to_le_bytes());
    preferences[6..8].copy_from_slice(&25u16.to_le_bytes());
    preferences[8] = 1;
    preferences[12..20].copy_from_slice(&1u64.to_le_bytes());
    preferences[20..28].copy_from_slice(&2u64.to_le_bytes());
    let library = music_protocol::catalogue::identity::parse(&preferences).unwrap();
    // Cancelled: any connection attempt would fail on it, not on the check.
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(true)),
    };
    let pair = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let record = |signature| Unfinished {
        send: &send,
        operation: b"operation",
        signature,
    };
    let run = temp.0.join("run");
    assert!(
        remove_unfinished(&identity, &library, &run, &[], record(None), &control)
            .await
            .unwrap()
            .is_empty()
    );
    for (staged, signature, message) in [
        (
            pair(&["Sync_00000000.plist", "Other.plist"]),
            Some([1; 21]),
            "not an operation pair",
        ),
        (pair(&["Sync_00000000.plist"]), None, "no signature"),
    ] {
        let error = remove_unfinished(
            &identity,
            &library,
            &run,
            &staged,
            record(signature),
            &control,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
    }
    traced(&send, "001-outgoing.atc", "FinishedSyncingMetadata");
    let error = remove_unfinished(
        &identity,
        &library,
        &run,
        &pair(&["Sync_00000000.plist", "Sync_00000000.plist.cig"]),
        record(Some([1; 21])),
        &control,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("sent FinishedSyncingMetadata"),
        "{error:#}"
    );
    assert!(!run.exists());
}
