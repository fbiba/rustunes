//! Cover requests use the same explicit engine cancellation boundary.
mod common;
use rustunes::engine::CoverRequest;
use std::path::Path;

#[tokio::test]
async fn cancelled_cover_refuses_before_opening_journal_or_creating_evidence() {
    let path = Path::new("this-artwork-test-input-must-not-be-read");
    let result = common::engine(true)
        .cover(CoverRequest {
            file: path,
            snapshot: path,
            journal: path,
            trace: path,
            host_name: "test frontend",
            dry_run: false,
            expected_source: None,
            preserve_album: None,
            pending_cover_only: false,
        })
        .await;
    assert!(result.err().unwrap().to_string().contains("cancelled"));
    assert!(!path.exists());
}
