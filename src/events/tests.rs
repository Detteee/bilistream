use super::*;

fn event() -> EventKind {
    EventKind::QuorumChanged { has_quorum: true }
}

#[test]
fn preseal_full_closed_and_independent_consumers_are_counted() {
    let bus = Bus::new();
    bus.emit_at(1, event());
    let mut stalled = bus.register("history", 1, |_| true);
    let mut notify = bus.register("notify", 2, |k| {
        matches!(k, EventKind::QuorumChanged { .. })
    });
    bus.seal();
    bus.emit_at(2, event());
    let first = notify.try_recv().unwrap();
    assert_eq!(first.at, 2);
    bus.emit_at(3, event());
    assert_eq!(
        notify.try_recv().unwrap().at,
        3,
        "a full history queue is independent"
    );
    assert_eq!(stalled.try_recv().unwrap().at, 2);
    drop(stalled);
    bus.emit_at(4, event());
    assert_eq!(notify.try_recv().unwrap().at, 4);
    assert_eq!(
        bus.dropped(),
        vec![("unsealed", 1), ("history", 2), ("notify", 0)]
    );
}

#[test]
fn one_event_is_shared_and_registration_cannot_replace_a_sealed_receiver() {
    let bus = Bus::new();
    let mut first = bus.register("first", 2, |_| true);
    let mut second = bus.register("second", 2, |_| true);
    bus.seal();
    let mut replacement = bus.register("first", 2, |_| true);
    assert!(matches!(
        replacement.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    bus.emit_at(10, event());
    let a = first.try_recv().unwrap();
    let b = second.try_recv().unwrap();
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn startup_fact_precedes_the_consumer_ready_barrier() {
    let bus = Bus::new();
    let mut rx = bus.register("history", 4, |_| true);
    bus.seal_with_initial(Some(Event {
        at: 1,
        kind: EventKind::ProcessStarted { version: "test" },
    }));
    assert!(bus.initialized.load(Ordering::Acquire));
    bus.emit_at(2, EventKind::RecorderHeartbeat);
    assert!(matches!(
        rx.try_recv().unwrap().kind,
        EventKind::ProcessStarted { .. }
    ));
    assert!(matches!(
        rx.try_recv().unwrap().kind,
        EventKind::RecorderHeartbeat
    ));
    bus.seal_with_initial(Some(Event {
        at: 3,
        kind: event(),
    }));
    assert!(
        rx.try_recv().is_err(),
        "sealing twice must not repeat startup"
    );
}

#[test]
fn synchronous_and_foreign_runtime_producers_do_not_wait_for_consumers() {
    let bus = Arc::new(Bus::new());
    let mut rx = bus.register("reader", 2, |_| true);
    bus.seal();
    with_sync_test_bus(Arc::clone(&bus), || emit_at(3, event()));
    let other = Arc::clone(&bus);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(with_test_bus(other, async {
            emit_at(4, event());
        }));
    })
    .join()
    .unwrap();
    assert_eq!(rx.try_recv().unwrap().at, 3);
    assert_eq!(rx.try_recv().unwrap().at, 4);
    for _ in 0..100_000 {
        bus.emit_at(5, event());
    }
    assert_eq!(
        bus.dropped()
            .iter()
            .find(|(n, _)| *n == "reader")
            .unwrap()
            .1,
        99_998
    );
}

#[test]
#[ignore = "nonblocking bus latency acceptance measurement"]
fn saturated_emit_latency() {
    let bus = Bus::new();
    let _stalled = bus.register("history", 1, |_| true);
    let mut notify = bus.register("notify", 64, |event| {
        matches!(event, EventKind::BroadcastEnded)
    });
    bus.seal();
    bus.emit_at(0, event());
    let begin = std::time::Instant::now();
    for at in 1..=100_000 {
        bus.emit_at(at, event());
    }
    let bulk = begin.elapsed();
    let mut samples = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let at = std::time::Instant::now();
        bus.emit_at(100_001, event());
        samples.push(at.elapsed().as_nanos());
    }
    samples.sort_unstable();
    bus.emit_at(100_002, EventKind::BroadcastEnded);
    assert!(matches!(
        notify.try_recv().unwrap().kind,
        EventKind::BroadcastEnded
    ));
    assert_eq!(
        bus.dropped(),
        vec![("unsealed", 0), ("history", 110_001), ("notify", 0)]
    );
    eprintln!(
        "bus saturated100k_ms={} median_ns={} p95_ns={}",
        bulk.as_secs_f64() * 1000.0,
        samples[5000],
        samples[9500]
    );
    assert!(bulk < std::time::Duration::from_millis(50));
    assert!(samples[5000] < 1000);
}
