//! Cancelled requests stop at the public engine seam before any local/device work.
mod common;
use rustunes::engine::ChangeApplyRequest;
use std::path::Path;

#[tokio::test]
async fn cancelled_apply_refuses_before_reading_plan_or_reserving_an_attempt() {
    let path = Path::new("this-engine-test-input-must-not-be-read");
    let result = common::engine(true)
        .apply_change(
            ChangeApplyRequest {
                plan: path,
                snapshot: path,
                journal: path,
                trace: path,
                state: path,
                host_name: "test frontend",
                seconds: 1,
                batch_size: 1,
                check_only: false,
                dry_run: false,
            },
            &mut |_| Ok(true),
        )
        .await;
    assert!(result.err().unwrap().to_string().contains("cancelled"));
    assert!(!path.exists());
}
