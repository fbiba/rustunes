use super::*;
use std::time::Duration;

#[test]
fn enclosing_deadline_and_cancellation_are_shared_without_renewal() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(5);
    let engine = Engine::quiet(Config {
        device: DeviceConfig::default(),
        deadline: Some(deadline),
        cancelled: Arc::clone(&cancelled),
    });
    let first = engine.control(120);
    let second = engine.control(1800);
    assert_eq!(first.deadline, deadline);
    assert_eq!(second.deadline, deadline);
    assert!(Arc::ptr_eq(&first.cancelled, &cancelled));
    assert!(Arc::ptr_eq(&second.cancelled, &cancelled));
    cancelled.store(true, Ordering::Relaxed);
    assert!(first.check().is_err());
    assert!(engine.check().is_err());
}

#[tokio::test]
async fn nested_engine_observers_remain_scoped_to_their_caller() {
    let outer_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inner_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outer_sink = Arc::clone(&outer_count);
    let inner_sink = Arc::clone(&inner_count);
    let outer = Engine::new(test_engine().config, move |_| {
        outer_sink.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    let inner = Engine::new(test_engine().config, move |_| {
        inner_sink.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    outer
        .scope(async {
            observe(Event::ReadingSavedSnapshot).unwrap();
            inner
                .scope(async {
                    observe(Event::ComparingSources).unwrap();
                })
                .await;
            observe(Event::ReadingSavedSnapshot).unwrap();
        })
        .await;
    observe(Event::ComparingSources).unwrap();
    assert_eq!(outer_count.load(Ordering::Relaxed), 2);
    assert_eq!(inner_count.load(Ordering::Relaxed), 1);
}
