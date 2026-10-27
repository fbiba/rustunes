//! Read-only recovery remains behind the application's typed cancellation boundary.
mod common;
use rustunes::engine::RecoveryRequest;
use std::path::Path;

#[tokio::test]
async fn cancelled_recovery_never_opens_inputs_or_creates_trace() {
    let path = Path::new("this-recovery-test-input-must-not-be-read");
    let result = common::engine(true)
        .recover(RecoveryRequest {
            snapshot: path,
            journal: path,
            run_dir: None,
            trace: path,
            cancel_prepared: None,
            selected_device: None,
        })
        .await;
    assert!(result.err().unwrap().to_string().contains("cancelled"));
    assert!(!path.exists());
}
