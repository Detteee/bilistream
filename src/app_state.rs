use std::collections::VecDeque;
use std::sync::{Arc, LockResult, Mutex, OnceLock, RwLock};
use tokio::sync::{broadcast, Notify};

use crate::webui::state::StatusData;

const LOG_CAPACITY: usize = 500;
const EVENT_BUS_CAPACITY: usize = 64;

static PROCESS: OnceLock<AppState> = OnceLock::new();

/// Process runtime created in `main` and passed into the Web UI.
///
/// Distinct [`AppState::new`] values do not share logs, status, or events.
/// Free-function shims such as [`crate::webui::state::update_status_cache`]
/// still go through [`AppState::current`], which is the instance [`install`]ed
/// at startup (or a lazily created process default in tests).
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    logs: Mutex<Option<VecDeque<String>>>,
    status: RwLock<Option<StatusData>>,
    status_refresh: Notify,
    events: broadcast::Sender<&'static str>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                logs: Mutex::new(None),
                status: RwLock::new(None),
                status_refresh: Notify::new(),
                events: broadcast::channel(EVENT_BUS_CAPACITY).0,
            }),
        }
    }

    /// Make this the process-wide default used by free-function shims.
    ///
    /// The first call wins; later calls return the instance already installed.
    pub fn install(self) -> Self {
        match PROCESS.set(self.clone()) {
            Ok(()) => self,
            Err(_) => PROCESS
                .get()
                .cloned()
                .expect("process AppState is installed"),
        }
    }

    pub fn current() -> Self {
        PROCESS
            .get()
            .cloned()
            .unwrap_or_else(|| Self::new().install())
    }

    pub fn init_log_buffer(&self) {
        let mut buffer = recover_lock(self.inner.logs.lock(), "webui log buffer");
        *buffer = Some(VecDeque::with_capacity(LOG_CAPACITY));
    }

    pub fn add_log_line(&self, line: String) {
        let mut buffer = recover_lock(self.inner.logs.lock(), "webui log buffer");
        if let Some(ref mut buf) = *buffer {
            buf.push_back(line);
            if buf.len() > LOG_CAPACITY {
                buf.pop_front();
            }
        }
    }

    pub fn get_logs(&self) -> Vec<String> {
        let buffer = recover_lock(self.inner.logs.lock(), "webui log buffer");
        if let Some(ref buf) = *buffer {
            buf.iter().cloned().collect()
        } else {
            Vec::new()
        }
    }

    pub fn update_status_cache(&self, status: StatusData) {
        let mut cache = recover_lock(self.inner.status.write(), "webui status cache");
        let changed = cache.as_ref() != Some(&status);
        *cache = Some(status);
        drop(cache);

        if changed {
            self.publish(crate::webui::events::STATUS);
        }
    }

    pub fn update_status_cache_with(&self, update: impl FnOnce(&mut StatusData)) {
        let mut cache = recover_lock(self.inner.status.write(), "webui status cache");
        let status = cache.get_or_insert_with(StatusData::default);
        let before = status.clone();
        update(status);
        let changed = *status != before;
        drop(cache);

        if changed {
            self.publish(crate::webui::events::STATUS);
        }
    }

    pub fn get_status_cache(&self) -> Option<StatusData> {
        let cache = recover_lock(self.inner.status.read(), "webui status cache");
        cache.clone()
    }

    pub fn request_status_refresh(&self) {
        self.inner.status_refresh.notify_one();
    }

    pub async fn status_refresh_requested(&self) {
        self.inner.status_refresh.notified().await;
    }

    pub fn publish(&self, kind: &'static str) {
        let _ = self.inner.events.send(kind);
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<&'static str> {
        self.inner.events.subscribe()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn recover_lock<T>(lock: LockResult<T>, name: &str) -> T {
    lock.unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {}", name);
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::state::BiliStatus;

    #[test]
    fn distinct_app_states_do_not_share_status_or_logs() {
        let a = AppState::new();
        let b = AppState::new();
        a.init_log_buffer();
        b.init_log_buffer();

        a.add_log_line("from-a".to_string());
        a.update_status_cache(StatusData {
            bilibili: BiliStatus {
                title: "from-a".to_string(),
                ..BiliStatus::default()
            },
            ..StatusData::default()
        });

        assert_eq!(a.get_logs(), vec!["from-a".to_string()]);
        assert!(b.get_logs().is_empty());
        assert_eq!(
            a.get_status_cache().expect("status written").bilibili.title,
            "from-a"
        );
        assert!(b.get_status_cache().is_none());
    }

    #[test]
    fn distinct_app_states_do_not_share_event_bus() {
        let a = AppState::new();
        let b = AppState::new();
        let mut a_rx = a.subscribe_events();
        let mut b_rx = b.subscribe_events();

        a.publish("status");

        assert_eq!(a_rx.try_recv().expect("event on a"), "status");
        assert!(b_rx.try_recv().is_err());
    }
}
