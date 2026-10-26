use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
fn row(id: &str) -> Listed {
    Listed {
        udid: id.into(),
        transport: Transport::Usb,
    }
}
#[tokio::test]
async fn listing_reports_exact_inventory_without_pairing_claims() {
    let queries = AtomicUsize::new(0);
    let mut output = Vec::new();
    list_with(
        true,
        &mut output,
        Instant::now() + Duration::from_secs(1),
        || async {
            queries.fetch_add(1, Ordering::Relaxed);
            Ok(vec![row("AAAA"), row("BBBB")])
        },
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(queries.load(Ordering::Relaxed), 1);
    assert_eq!(value["devices"][0]["udid"], "AAAA");
    assert_eq!(value["devices"][1]["udid"], "BBBB");
    assert_eq!(value["device_count"], 2);
    assert_eq!(value["selection_required"], true);
    assert_eq!(value["pairing_checked"], false);
    assert_eq!(value["phone_writes"], 0);
    output.clear();
    list_with(
        false,
        &mut output,
        Instant::now() + Duration::from_secs(1),
        || async { Ok(Vec::new()) },
    )
    .await
    .unwrap();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("No Apple devices")
    );
}
#[tokio::test]
async fn failed_or_expired_query_emits_no_partial_json_and_never_retries() {
    let queries = AtomicUsize::new(0);
    let mut output = Vec::new();
    assert!(
        list_with(true, &mut output, Instant::now(), || async {
            queries.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .await
        .is_err()
    );
    assert_eq!(queries.load(Ordering::Relaxed), 0);
    assert!(output.is_empty());
    assert!(
        list_with(
            true,
            &mut output,
            Instant::now() + Duration::from_secs(1),
            || async {
                queries.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("synthetic mux failure")
            }
        )
        .await
        .is_err()
    );
    assert_eq!(queries.load(Ordering::Relaxed), 1);
    assert!(output.is_empty());
    let deadline = Instant::now() + Duration::from_millis(10);
    assert!(
        list_with(true, &mut output, deadline, || async {
            std::future::pending::<Result<Vec<Listed>>>().await
        })
        .await
        .is_err()
    );
    assert!(output.is_empty());
}
#[tokio::test]
async fn human_identifier_output_escapes_terminal_controls() {
    let mut output = Vec::new();
    list_with(
        false,
        &mut output,
        Instant::now() + Duration::from_secs(1),
        || async { Ok(vec![row("bad\u{1b}[31m\n\u{202e}")]) },
    )
    .await
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{202e}'));
    assert!(text.contains("\\n"));
    assert!(text.contains("Pairing status has not been checked"));
}
#[tokio::test]
async fn wifi_rows_carry_their_address() {
    let mut output = Vec::new();
    list_with(
        true,
        &mut output,
        Instant::now() + Duration::from_secs(1),
        || async {
            Ok(vec![Listed {
                udid: "AAAA".into(),
                transport: Transport::Wifi("192.0.2.16".parse().unwrap()),
            }])
        },
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["devices"][0]["connection"], "wifi");
    assert_eq!(value["devices"][0]["address"], "192.0.2.16");
    output.clear();
    list_with(
        false,
        &mut output,
        Instant::now() + Duration::from_secs(1),
        || async { Ok(vec![row("AAAA")]) },
    )
    .await
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("AAAA (USB)"));
}
