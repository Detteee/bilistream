//! Runtime-independent nonblocking facts shared by history and notifications.
//! Registration is startup-only; hot-path emission reads an immutable registry.

pub mod event;
pub use event::{
    BiliLiveRecord, BiliSummaryPhase, BiliTotals, EndReason, Event, EventKind, NodeState, Severity,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;

struct Consumer {
    name: &'static str,
    filter: fn(&EventKind) -> bool,
    tx: mpsc::Sender<Arc<Event>>,
    dropped: AtomicU64,
}

/// An instance is also used by isolation tests, without changing the process
/// registry shared by unrelated tests.
pub(crate) struct Bus {
    pending: Mutex<Vec<Consumer>>,
    sealed: OnceLock<Box<[Consumer]>>,
    unsealed: AtomicU64,
    ready: tokio::sync::Notify,
    initialized: AtomicBool,
}

impl Bus {
    pub(crate) const fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            sealed: OnceLock::new(),
            unsealed: AtomicU64::new(0),
            ready: tokio::sync::Notify::const_new(),
            initialized: AtomicBool::new(false),
        }
    }

    pub(crate) fn register(
        &self,
        name: &'static str,
        cap: usize,
        filter: fn(&EventKind) -> bool,
    ) -> mpsc::Receiver<Arc<Event>> {
        let (tx, rx) = mpsc::channel(cap.max(1));
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if self.sealed.get().is_some() || pending.iter().any(|c| c.name == name) {
            // A repeated listener start must not replace a live consumer.
            // The caller gets a closed receiver, rather than a second queue.
            return rx;
        }
        pending.push(Consumer {
            name,
            filter,
            tx,
            dropped: AtomicU64::new(0),
        });
        rx
    }

    #[cfg(test)]
    pub(crate) fn seal(&self) {
        self.seal_with_initial(None);
    }

    fn seal_with_initial(&self, initial: Option<Event>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if self.sealed.get().is_none() {
            let _ = self.sealed.set(std::mem::take(&mut *pending).into());
            if let Some(event) = initial {
                self.emit_at(event.at, event.kind);
            }
            self.initialized.store(true, Ordering::Release);
            self.ready.notify_waiters();
        }
    }

    pub(crate) fn emit_at(&self, at: u64, kind: EventKind) {
        let Some(consumers) = self.sealed.get() else {
            self.unsealed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let event = Arc::new(Event { at, kind });
        for consumer in consumers {
            if (consumer.filter)(&event.kind) && consumer.tx.try_send(Arc::clone(&event)).is_err() {
                consumer.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn dropped(&self) -> Vec<(&'static str, u64)> {
        let mut counts = vec![("unsealed", self.unsealed.load(Ordering::Relaxed))];
        if let Some(consumers) = self.sealed.get() {
            counts.extend(
                consumers
                    .iter()
                    .map(|c| (c.name, c.dropped.load(Ordering::Relaxed))),
            );
        }
        counts
    }
}

static BUS: Bus = Bus::new();

pub fn register(
    name: &'static str,
    cap: usize,
    filter: fn(&EventKind) -> bool,
) -> mpsc::Receiver<Arc<Event>> {
    BUS.register(name, cap, filter)
}

pub fn seal() {
    BUS.seal_with_initial(Some(Event {
        at: now(),
        kind: EventKind::ProcessStarted {
            version: env!("CARGO_PKG_VERSION"),
        },
    }));
}

pub fn is_sealed() -> bool {
    BUS.initialized.load(Ordering::Acquire)
}

/// Consumer supervisors wait here before their initial observation. Register
/// first, then seal, then emit the initial process/recorder facts exactly once.
pub async fn wait_sealed() {
    loop {
        let ready = BUS.ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();
        if is_sealed() {
            return;
        }
        ready.await;
    }
}

pub fn emit(kind: EventKind) {
    emit_at(now(), kind);
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn emit_at(at: u64, kind: EventKind) {
    #[cfg(test)]
    if let Some(bus) = TEST_BUS
        .try_with(Arc::clone)
        .ok()
        .or_else(|| THREAD_BUS.with(|slot| slot.borrow().clone()))
    {
        bus.emit_at(at, kind);
        return;
    }
    BUS.emit_at(at, kind);
}

pub fn dropped() -> Vec<(&'static str, u64)> {
    #[cfg(test)]
    if let Some(bus) = TEST_BUS
        .try_with(Arc::clone)
        .ok()
        .or_else(|| THREAD_BUS.with(|slot| slot.borrow().clone()))
    {
        return bus.dropped();
    }
    BUS.dropped()
}

#[cfg(test)]
tokio::task_local! {
    static TEST_BUS: Arc<Bus>;
}

#[cfg(test)]
thread_local! {
    static THREAD_BUS: std::cell::RefCell<Option<Arc<Bus>>> = const { std::cell::RefCell::new(None) };
}

/// Drive real producers against an isolated bus without changing startup's
/// process-wide OnceLock or unrelated parallel tests.
#[cfg(test)]
pub(crate) async fn with_test_bus<F: std::future::Future>(bus: Arc<Bus>, future: F) -> F::Output {
    TEST_BUS.scope(bus, future).await
}

#[cfg(test)]
pub(crate) fn with_sync_test_bus<T>(bus: Arc<Bus>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Arc<Bus>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THREAD_BUS.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(THREAD_BUS.with(|slot| slot.replace(Some(bus))));
    f()
}

#[cfg(test)]
mod tests;
