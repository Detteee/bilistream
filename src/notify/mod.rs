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
pub mod credentials;
pub mod event;
pub mod format;
pub mod ledger;
pub mod live;
pub mod live_events;
pub mod observer;
pub mod openid_sync;
pub mod policy;
pub mod qq;
pub mod readiness;
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
        reply: oneshot::Sender<Result<serde_json::Value, Refusal>>,
    },
    CancelBind,
    /// A one-shot event. Dropped when the channel is full.
    #[allow(dead_code)]
    Event(event::AlertEvent),
    /// 开播/下播 from the streaming runtime.
    Live(live_events::LiveSignal),
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
    /// The single openid writer as the worker last saw it (`None` = no
    /// node has authority). Only meaningful while the worker runs.
    pub writer: Option<NodeRef>,
    /// This node holds a QQ gateway session (heartbeat `notify_gateway_held`).
    pub gateway_held: bool,
    /// 1b `/status.nodes[]` and heartbeat booleans (never the values).
    pub appsecret_set: bool,
    pub appid_set: bool,
    pub openid_set: bool,
    /// Addendum F: this node's credential set generation, and whether its
    /// stored openid is bound under another AppID (never the values).
    pub credentials_generation: u64,
    pub openid_rebind: bool,
}

/// `{node_id, is_local}` as `/status` shows `openid_writer` and
/// `gateway_owner`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NodeRef {
    pub node_id: String,
    pub is_local: bool,
}

/// A refused notify write: HTTP 409 with
/// `{success:false, message, data:{code, ...}}` (contract "Refusal codes").
/// Clients branch only on `data.code`.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub message: String,
    pub data: serde_json::Value,
}

impl Refusal {
    pub fn code(&self) -> &str {
        self.data["code"].as_str().unwrap_or_default()
    }

    pub fn simple(code: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: serde_json::json!({ "code": code }),
        }
    }

    /// `action` completes 「请打开发送节点 X 的面板…」, e.g. 进行绑定.
    pub fn not_owner(owner: &str, action: &str) -> Self {
        Self {
            message: format!("请打开发送节点 {owner} 的面板{action}"),
            data: serde_json::json!({ "code": "not_owner", "owner": owner }),
        }
    }

    /// `action` completes 「暂无可写节点，暂时无法…」, e.g. 绑定.
    pub fn no_writer(action: &str) -> Self {
        Self::simple("no_writer", format!("暂无可写节点，暂时无法{action}"))
    }

    pub fn revision_conflict(current: u64) -> Self {
        Self {
            message: "配置已被其他操作修改，请重新加载后再保存".to_string(),
            data: serde_json::json!({ "code": "revision_conflict", "current_revision": current }),
        }
    }
}

/// Is the worker task alive (its command channel open)?
pub fn worker_running() -> bool {
    COMMANDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|sender| !sender.is_closed())
}

/// The single-writer check for openid writes made outside the worker
/// (`PUT admin_openid`, `DELETE /api/notify/openid`). A stopped worker
/// means unknown, which the server answers with `no_writer`.
pub fn check_writer(action: &str) -> Result<(), Refusal> {
    let writer = {
        let shared = shared();
        if shared.status.is_null() {
            None
        } else {
            shared.writer.clone()
        }
    };
    if !worker_running() {
        return Err(Refusal::no_writer(action));
    }
    writer_allows(writer.as_ref(), action)
}

/// The writer rule itself: local writer passes, another writer is
/// `not_owner`, no writer is `no_writer`. No gateway is needed.
pub fn writer_allows(writer: Option<&NodeRef>, action: &str) -> Result<(), Refusal> {
    match writer {
        Some(writer) if writer.is_local => Ok(()),
        Some(writer) => Err(Refusal::not_owner(&writer.node_id, action)),
        None => Err(Refusal::no_writer(action)),
    }
}

pub static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Default::default);

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The heartbeat worker recomputed cluster status. Coalescing, never blocks.
pub fn cluster_changed() {
    WAKE.notify_one();
}

/// Successful start-live (Addendum A). Never waits; a stopped notifier
/// ignores it.
pub fn live_started(info: live_events::LiveStartInfo) {
    let _ = send_command(Command::Live(live_events::LiveSignal::Started(info)));
}

/// 直播结束 on the active node (the same signal as the status page's 切换).
pub fn live_ended() {
    let _ = send_command(Command::Live(live_events::LiveSignal::Ended));
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

/// `(appsecret_set, openid_set)` on this node, as the worker last saw them.
pub fn local_credential_flags() -> (bool, bool) {
    let shared = shared();
    (shared.appsecret_set, shared.openid_set)
}

/// `(credentials_generation, openid_rebind)` on this node (Addendum F).
pub fn local_credentials_sync() -> (u64, bool) {
    let shared = shared();
    (shared.credentials_generation, shared.openid_rebind)
}

/// This node's id as the worker last saw it.
pub fn local_node_id() -> String {
    shared().ledger.local.clone()
}

/// This node has a saved AppID (Addendum C).
pub fn local_appid_set() -> bool {
    shared().appid_set
}

/// This node holds a QQ gateway session right now.
pub fn local_gateway_held() -> bool {
    shared().gateway_held
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
