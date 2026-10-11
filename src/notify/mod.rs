//! QQ notifications for cluster node status (task 10-11-qq-bot-notifications).
//!
//! Hot paths only call `cluster_changed()`: one `Notify::notify_one`, no
//! allocation, no lock. The worker reads the cluster state itself, decides
//! the sender, applies the anti-spam policy and talks to QQ. A dead or slow
//! notifier can never delay a heartbeat or a stream.

pub mod authority;
pub mod bind;
pub mod commands;
pub mod config;
pub mod event;
pub mod format;
pub mod ledger;
pub mod live;
pub mod observer;
pub mod policy;
pub mod qq;
pub mod sink;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub mod worker;

pub use config::NotifyConfig;
pub use ledger::NotifyLedger;

use std::sync::{LazyLock, Mutex};
use tokio::sync::{mpsc, oneshot, Notify};

/// Commands from the WebUI and (Phase 3) local event producers.
pub enum Command {
    /// Settings or secret changed.
    Reload,
    Test {
        reply: oneshot::Sender<worker::TestReport>,
    },
    StartBind {
        mode: bind::BindMode,
        /// The panel confirmed replacing an already bound openid.
        replace: bool,
        reply: oneshot::Sender<Result<serde_json::Value, worker::BindRefusal>>,
    },
    CancelBind,
    /// A one-shot event. Dropped when the channel is full.
    #[allow(dead_code)]
    Event(event::AlertEvent),
}

pub(crate) const COMMAND_CAPACITY: usize = 64;

static WAKE: Notify = Notify::const_new();
/// Replaced when a stopped worker is started again (WebUI rebind).
static COMMANDS: Mutex<Option<mpsc::Sender<Command>>> = Mutex::new(None);

/// What other modules (heartbeat snapshot, status API) read.
#[derive(Default)]
pub struct Shared {
    pub ledger: NotifyLedger,
    pub capable: bool,
    pub authority: bool,
    pub status: serde_json::Value,
}

pub static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Default::default);

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The heartbeat worker recomputed cluster status. Coalescing, never blocks.
pub fn cluster_changed() {
    WAKE.notify_one();
}

/// Ledger to advertise in this node's heartbeat snapshot.
pub fn heartbeat_ledger() -> Option<NotifyLedger> {
    let shared = shared();
    let mut ledger = shared.ledger.clone();
    ledger.local.clear();
    (ledger != NotifyLedger::default()).then_some(ledger)
}

/// Enabled with AppID, AppSecret and admin openid.
pub fn local_capable() -> bool {
    shared().capable
}

/// This node is the notify sender right now.
pub fn local_authority() -> bool {
    shared().authority
}

pub fn status_view() -> serde_json::Value {
    let status = shared().status.clone();
    if status.is_null() {
        serde_json::json!({"worker": "stopped"})
    } else {
        status
    }
}

/// Queue a command for the worker. Fails when the worker is not running or
/// is saturated; never waits.
pub fn send_command(command: Command) -> Result<(), &'static str> {
    let sender = COMMANDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or("通知服务未启动")?;
    sender.try_send(command).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => "通知服务繁忙，请稍后重试",
        mpsc::error::TrySendError::Closed(_) => "通知服务未运行",
    })?;
    WAKE.notify_one();
    Ok(())
}

/// The running supervisor; dropping it stops the notifier (and the gateway).
pub struct NotifyWorker(tokio::task::JoinHandle<()>);

impl Drop for NotifyWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Start the supervisor unless one is already running.
pub fn start() -> Option<NotifyWorker> {
    let mut slot = COMMANDS.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|sender| !sender.is_closed()) {
        return None;
    }
    let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
    *slot = Some(sender);
    Some(NotifyWorker(tokio::spawn(worker::supervise(
        receiver, &WAKE,
    ))))
}
