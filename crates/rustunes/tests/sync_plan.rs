//! Frontend reporting failure propagates before opening the saved snapshot.
use rustunes::engine::{Config, DeviceConfig, Engine, Event, PlanRequest, SelectionOptions};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

#[test]
fn observer_error_stops_planning_before_reading_the_snapshot() {
    let engine = Engine::new(
        Config {
            device: DeviceConfig::default(),
            deadline: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        },
        |event| match event {
            Event::ReadingSavedSnapshot => anyhow::bail!("frontend output closed"),
            _ => Ok(()),
        },
    );
    let selection = SelectionOptions::default();
    let path = Path::new("this-plan-test-input-must-not-be-read");
    let result = engine.plan(PlanRequest {
        source: path,
        snapshot: path,
        journal: None,
        output: None,
        selection: &selection,
        add_only: true,
        flac_to_alac: false,
    });
    assert_eq!(result.err().unwrap().to_string(), "frontend output closed");
    assert!(!path.exists());
}
