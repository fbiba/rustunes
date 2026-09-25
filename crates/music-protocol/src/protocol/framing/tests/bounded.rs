use super::*;
use crate::protocol::{trace::OutputAllowance, transfer::CaptureControl};
use std::sync::{Arc, atomic::AtomicBool};
use tokio::time::Instant;

fn control() -> CaptureControl {
    CaptureControl {
        deadline: Instant::now() + std::time::Duration::from_secs(5),
        cancelled: Arc::new(AtomicBool::new(false)),
    }
}
async fn bounded_pair(
    output: &OutputAllowance,
    control: &CaptureControl,
    capacity: usize,
) -> (Atc, tokio::io::DuplexStream) {
    let (client, peer) = duplex(capacity);
    (
        Atc::from_stream(Idevice::new(Box::new(client), "offline-only"))
            .await
            .unwrap()
            .with_output(output.clone(), control)
            .unwrap(),
        peer,
    )
}
fn trace_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}
fn frame(value: &Value) -> Vec<u8> {
    let mut body = Vec::new();
    value.to_writer_binary(&mut body).unwrap();
    [(body.len() as u32).to_le_bytes().as_slice(), &body].concat()
}

#[tokio::test]
async fn bounded_outgoing_accounts_raw_and_xml_before_exact_transmission() {
    let dir = new_trace_dir("bounded-success");
    let output = OutputAllowance::new(4096, 2);
    let (mut atc, mut peer) = bounded_pair(&output, &control(), 4096).await;
    let value = Value::String("<&> bounded outgoing".into());
    atc.send_plist(&value, &dir, 0).await.unwrap();
    drop(atc);
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).await.unwrap();
    assert_eq!(sent, frame(&value));
    assert_eq!(fs::read(dir.join("000-outgoing.atc")).unwrap(), sent);
    assert_eq!(
        Value::from_file(dir.join("000-outgoing.plist")).unwrap(),
        value
    );
    let usage = output.usage().unwrap();
    assert_eq!(usage.charged_bytes, trace_size(&dir));
    assert_eq!(usage.charged_files, 2);
    for entry in fs::read_dir(&dir).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o400
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn expanded_xml_exhaustion_retains_raw_and_prefix_without_sending() {
    let dir = new_trace_dir("bounded-xml");
    let value = Value::String("<&>".repeat(512));
    let intended = frame(&value);
    let output = OutputAllowance::new(intended.len() as u64 + 256, 2);
    let (mut atc, mut peer) = bounded_pair(&output, &control(), 4096).await;
    assert!(atc.send_plist(&value, &dir, 0).await.is_err());
    assert!(
        atc.send_plist(&Value::Boolean(true), &dir, 1)
            .await
            .is_err()
    );
    drop(atc);
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).await.unwrap();
    assert!(sent.is_empty());
    assert_eq!(fs::read(dir.join("000-outgoing.atc")).unwrap(), intended);
    let xml = fs::read(dir.join("000-outgoing.plist")).unwrap();
    assert!(!xml.is_empty() && xml.len() <= 256);
    assert_eq!(output.usage().unwrap().charged_bytes, trace_size(&dir));
    assert_eq!(output.usage().unwrap().charged_files, 2);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn bounded_binary_encoding_stops_at_allowance_and_preserves_encoded_prefix() {
    let dir = new_trace_dir("bounded-binary");
    let output = OutputAllowance::new(64, 2);
    let (mut atc, mut peer) = bounded_pair(&output, &control(), 4096).await;
    assert!(
        atc.send_plist(&Value::Data(vec![1; 8192]), &dir, 0)
            .await
            .is_err()
    );
    drop(atc);
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).await.unwrap();
    assert!(sent.is_empty());
    let partial = fs::read(dir.join("000-outgoing.atc")).unwrap();
    assert!(partial.len() >= 4 && partial.len() <= 64);
    assert_eq!(&partial[..4], &[0; 4]);
    assert_eq!(output.usage().unwrap().charged_bytes, partial.len() as u64);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    assert!(!dir.join("000-outgoing.plist").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn cancelled_bounded_send_keeps_both_charges_and_refuses_reuse() {
    let dir = new_trace_dir("bounded-cancel");
    let output = OutputAllowance::new(4096, 4);
    let control = control();
    let (mut atc, mut peer) = bounded_pair(&output, &control, 8).await;
    let value = Value::String("partial transport write".into());
    {
        let mut send = std::pin::pin!(atc.send_plist(&value, &dir, 0));
        poll_fn(|cx| {
            assert!(send.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    let charged = output.usage().unwrap();
    assert_eq!(charged.charged_files, 2);
    assert_eq!(charged.charged_bytes, trace_size(&dir));
    assert!(atc.send_plist(&value, &dir, 1).await.is_err());
    assert_eq!(output.usage().unwrap(), charged);
    drop(atc);
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).await.unwrap();
    assert_eq!(sent, frame(&value)[..8]);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn bounded_outgoing_collision_consumes_attempt_without_overwrite_or_send() {
    let dir = new_trace_dir("bounded-collision");
    fs::write(dir.join("000-outgoing.atc"), b"retained").unwrap();
    let output = OutputAllowance::new(4096, 2);
    let (mut atc, mut peer) = bounded_pair(&output, &control(), 4096).await;
    assert!(
        atc.send_plist(&Value::Boolean(true), &dir, 0)
            .await
            .is_err()
    );
    drop(atc);
    let mut sent = Vec::new();
    peer.read_to_end(&mut sent).await.unwrap();
    assert!(sent.is_empty());
    assert_eq!(fs::read(dir.join("000-outgoing.atc")).unwrap(), b"retained");
    assert_eq!(output.usage().unwrap().charged_bytes, 0);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn bounded_atc_split_retains_shared_allowance_and_original_control() {
    let dir = new_trace_dir("bounded-split");
    let output = OutputAllowance::new(4096, 4);
    let original = control();
    let (atc, _) = bounded_pair(&output, &original, 16).await;
    let (_read, _write, carried) = atc.into_pending_parts(&original).unwrap();
    carried
        .unwrap()
        .write(&dir.join("same-allowance"), b"shared")
        .unwrap();
    assert_eq!(output.usage().unwrap().charged_bytes, 6);
    assert_eq!(output.usage().unwrap().charged_files, 1);
    let (atc, _) = bounded_pair(&output, &original, 16).await;
    let replaced = CaptureControl {
        deadline: original.deadline,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    assert!(atc.into_pending_parts(&replaced).is_err());
    fs::remove_dir_all(dir).unwrap();
}
