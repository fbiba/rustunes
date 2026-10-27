//! Alternative frontends need no Clap parser to provide recovery identity selection.
use rustunes::engine::{Config, DeviceConfig, Engine, RecoveryRequest};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

#[tokio::test]
async fn conflicting_recovery_selection_refuses_before_any_saved_or_device_input() {
    let engine = Engine::quiet(Config {
        device: DeviceConfig::new(Some("selected-device".into()), None).unwrap(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    });
    let path = Path::new("this-selected-device-test-input-must-not-be-read");
    let result = engine
        .recover(RecoveryRequest {
            snapshot: path,
            journal: path,
            run_dir: None,
            trace: path,
            cancel_prepared: None,
            selected_device: Some("different-device"),
        })
        .await;
    assert_eq!(
        result.err().unwrap().to_string(),
        "recovery device differs from engine configuration"
    );
    assert!(!path.exists());
}
