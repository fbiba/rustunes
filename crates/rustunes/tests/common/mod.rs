use rustunes::engine::{Config, DeviceConfig, Engine};
use std::sync::{Arc, atomic::AtomicBool};

pub fn engine(cancelled: bool) -> Engine {
    Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: None,
        cancelled: Arc::new(AtomicBool::new(cancelled)),
    })
}
