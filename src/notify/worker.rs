//! Notifier worker and supervisor (design §4–§8, review §0.2–§0.3).
//!
//! The supervisor owns the command receiver and restarts a panicked worker,
//! so the channel never closes under the WebUI. The worker loop waits on the
//! cluster wake-up, commands, gateway events and its own next deadline.
//! Retries are deadlines, never inline sleeps.

use super::authority::{self, Authority, DecideInput};
use super::bind::{BindDecision, BindMode, BindVia, Binder};
use super::commands;
use super::config::NotifyConfig;
use super::event::{AlertEvent, EventKind, ItemExtra, Phase};
use super::format::{self, RenderContext};
use super::ledger::{mask_reason, NotifyLedger};
use super::live::{self, LiveWatch};
use super::observer::{self, ClusterDigest};
use super::policy::{ItemKind, Mode, OutItem, PolicyConfig, Tracker};
use super::qq::api::{self, C2cMessage, QqMode};
use super::qq::gateway::{self, GatewayEvent, GatewayParams};
use super::qq::token::TokenCache;
use super::qq::{QqSink, QqTarget};
use super::sink::{DeliveryContext, PassiveTarget, Rendered, SendOutcome, Sink};
use super::store::{
    self, AppSecret, LastOutcome, OpenidChange, OpenidState, OpenidWrite, PersistedState,
};
use super::{Command, NodeRef, Refusal};
use crate::storage::Store;
use futures_util::FutureExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Wake at least this often even when nothing happens (fallback tick).
const IDLE_TICK_SECS: u64 = 15;
/// After startup, give heartbeats time to arrive before judging nodes.
const STARTUP_GRACE_SECS: u64 = 45;
const CONFIG_RELOAD_SECS: u64 = 30;
const STATE_SAVE_SECS: u64 = 30;
const RETRY_MIN_SECS: u64 = 15;
const RETRY_MAX_SECS: u64 = 600;
const GATEWAY_EVENTS: usize = 32;
/// The whole send ladder (token, passive, active, wakeup) must finish in
/// this long, so commands and gateway events never wait behind it.
const SEND_CAP_SECS: u64 = 25;
/// Keep the gateway this long after losing execution authority, so a
/// momentary sender-status flap does not reconnect the session.
const GATEWAY_HYSTERESIS_SECS: u64 = 30;
/// A failover seen during a handoff hold is announced once the hold
/// clears, but not if it takes longer than this.
const FAILOVER_LATCH_SECS: u64 = 3600;
/// Resolved items that could not be delivered wait for the next message.
const MAX_HELD: usize = 10;
const SUPERVISOR_BACKOFF_MIN: Duration = Duration::from_secs(5);
const SUPERVISOR_BACKOFF_MAX: Duration = Duration::from_secs(300);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TestReport {
    pub ok: bool,
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Warning in `/status.warning` when queued notifications are counted as
/// undelivered because no node is fully configured (contract: never a
/// silent drop).
pub const WARNING_NO_CAPABLE_NODE: &str = "no_capable_node";
const REASON_NO_CAPABLE_NODE: &str = "未配置完整：没有节点同时具备 AppSecret 和管理员 openid";

/// Aborts the gateway task when the handle is dropped (including when a
/// panicking worker unwinds).
struct GatewayTask(tokio::task::JoinHandle<()>);

impl Drop for GatewayTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn supervise(commands: mpsc::Receiver<Command>, wake: &'static Notify) {
    supervise_loop(commands, SUPERVISOR_BACKOFF_MIN, move |commands| {
        Box::pin(async move {
            let store = match crate::storage::global() {
                Ok(store) => store,
                Err(error) => {
                    tracing::warn!("QQ 通知无法打开数据库: {}", error);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return false;
                }
            };
            // Load the node id first, so the status never says "local".
            let node_id = crate::config::load_config()
                .await
                .ok()
                .map(|cfg| cfg.cluster.node_id)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| "local".to_string());
            let mut worker = Worker::new(store, node_id);
            worker.run(commands, wake).await;
            true
        })
    })
    .await
}

type RunOnce<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;

/// Owns the command receiver and restarts a panicked worker with backoff
/// (5 s doubling to 5 min; reset after 10 quiet minutes). `run_once`
/// returns `true` when the worker ended normally (channel closed).
pub(crate) async fn supervise_loop<F>(
    mut commands: mpsc::Receiver<Command>,
    min_backoff: Duration,
    mut run_once: F,
) where
    F: for<'a> FnMut(&'a mut mpsc::Receiver<Command>) -> RunOnce<'a>,
{
    let mut backoff = min_backoff;
    loop {
        let started = std::time::Instant::now();
        let run = AssertUnwindSafe(run_once(&mut commands)).catch_unwind();
        match run.await {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => {
                if started.elapsed() > Duration::from_secs(600) {
                    backoff = min_backoff;
                }
                tracing::error!("QQ 通知任务异常退出，{} 秒后重启", backoff.as_secs().max(1));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(SUPERVISOR_BACKOFF_MAX.max(min_backoff));
            }
        }
    }
}

/// Periodic catch-up pull (1b revision): a member that missed every push
/// retry converges without a restart.
pub(crate) const PULL_EVERY_SECS: u64 = 600;

/// 1b pull trigger mark: membership revision, writer-is-local, and when the
/// last pull started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PullMark {
    pub revision: u64,
    pub writer_local: bool,
    pub at: u64,
}

/// A pull is due on a new membership revision (joined, or first step after
/// a start), on becoming the writer, or `PULL_EVERY_SECS` after the last
/// one. Returns the new mark when a pull is due; `now` is the worker clock.
pub(crate) fn next_pull(
    last: Option<PullMark>,
    revision: u64,
    writer_local: bool,
    now: u64,
) -> Option<PullMark> {
    let due = match last {
        None => true,
        Some(mark) => {
            mark.revision != revision
                || (writer_local && !mark.writer_local)
                || now.saturating_sub(mark.at) >= PULL_EVERY_SECS
        }
    };
    due.then_some(PullMark {
        revision,
        writer_local,
        at: now,
    })
}

pub struct Worker {
    store: Arc<Store>,
    http: reqwest::Client,
    tokens: Arc<TokenCache>,
    node_id: String,
    cfg: NotifyConfig,
    cfg_rev: Option<u64>,
    secret: Option<AppSecret>,
    secret_rev: Option<u64>,
    /// Sealed `notify-openid` (never printed).
    openid: OpenidState,
    policy: PolicyConfig,
    tracker: Tracker,
    ledger: NotifyLedger,
    last_outcome: Option<LastOutcome>,
    binder: Binder,
    passive: Option<PassiveTarget>,
    ever_healthy: HashSet<String>,
    previous_owner: Option<String>,
    last_authority_seen: u64,
    started_at: u64,
    next_send_at: u64,
    retry_secs: u64,
    bad_credentials: bool,
    gateway: Option<GatewayTask>,
    gateway_state: (&'static str, u64, Option<String>),
    gateway_tx: mpsc::Sender<GatewayEvent>,
    gateway_rx: mpsc::Receiver<GatewayEvent>,
    authority: Authority,
    gateway_owner: bool,
    owner_hint: Option<String>,
    /// The single openid writer (`authority::openid_writer`).
    writer: Option<NodeRef>,
    /// The node holding (or entitled to hold) the QQ gateway session, as
    /// `/status.gateway_owner` shows it. Informational, except that bind
    /// refuses while it is another node than the writer (O6).
    gateway_holder: Option<NodeRef>,
    /// A peer whose heartbeat says it may still hold a gateway session
    /// (`authority::may_identify`); this node does not identify until it
    /// has closed, gone stale or been fenced.
    gateway_blocker: Option<String>,
    /// Queued items were counted undelivered: no node is fully configured.
    no_capable_node: bool,
    cluster_cfg: Option<crate::config::Config>,
    loaded_at: u64,
    dirty: bool,
    saved_at: u64,
    /// Last published gateway/bind/sender summary; a change pushes the SSE
    /// `status` event so the panel refreshes at once.
    signature: std::sync::Mutex<String>,
    /// `(old, new, reason, seen_at)`: an owner change seen in any mode,
    /// announced once the mode is Normal.
    pending_failover: Option<(String, String, String, u64)>,
    /// When this node stopped being the execution primary (hysteresis).
    lost_primary_at: Option<u64>,
    /// The gateway stopped for good (4013/4014/4914/4915) until settings change.
    gateway_fatal: Option<String>,
    /// Resolved items that could not be delivered; attached to the next one.
    held: Vec<OutItem>,
    live_watch: LiveWatch,
    live_now: Vec<live::LiveObservation>,
    limiter: commands::Limiter,
    was_membership_blocked: bool,
    /// 1b: `(membership revision, writer is local)` at the last openid pull.
    pull_key: Option<PullMark>,
    /// Items that expired while no node could send (H2 accounting).
    expired_no_sender: u32,
    warned_no_sender: bool,
    /// Bot/target fingerprint the sticky mode facts belong to.
    settings_fp: String,
    node_names: HashMap<String, String>,
    capable_nodes: (usize, usize),
    save_task: Option<tokio::task::JoinHandle<()>>,
}

impl Worker {
    pub fn new(store: Arc<Store>, node_id: String) -> Self {
        let mut persisted = store::load_state(&store);
        let (gateway_tx, gateway_rx) = mpsc::channel(GATEWAY_EVENTS);
        let now = now_secs();
        persisted.ledger.clamp_future(now);
        persisted.ledger.local = node_id.clone();
        let mut worker = Self {
            store,
            http: reqwest::Client::new(),
            tokens: Arc::new(TokenCache::default()),
            node_id,
            cfg: NotifyConfig::default(),
            cfg_rev: None,
            secret: None,
            secret_rev: None,
            openid: OpenidState::default(),
            policy: PolicyConfig::from_config(&NotifyConfig::default()),
            tracker: Tracker::default(),
            ledger: persisted.ledger,
            last_outcome: persisted.last_outcome,
            binder: Binder::default(),
            passive: None,
            ever_healthy: HashSet::new(),
            previous_owner: None,
            last_authority_seen: now,
            started_at: now,
            next_send_at: 0,
            retry_secs: RETRY_MIN_SECS,
            bad_credentials: false,
            gateway: None,
            gateway_state: ("off", now, None),
            gateway_tx,
            gateway_rx,
            authority: Authority::None,
            gateway_owner: false,
            owner_hint: None,
            writer: None,
            gateway_holder: None,
            gateway_blocker: None,
            no_capable_node: false,
            cluster_cfg: None,
            loaded_at: 0,
            dirty: false,
            saved_at: now,
            signature: std::sync::Mutex::new(String::new()),
            pending_failover: None,
            lost_primary_at: None,
            gateway_fatal: None,
            held: Vec::new(),
            live_watch: LiveWatch::default(),
            live_now: Vec::new(),
            limiter: commands::Limiter::default(),
            was_membership_blocked: false,
            pull_key: None,
            expired_no_sender: 0,
            warned_no_sender: false,
            settings_fp: persisted.settings,
            node_names: HashMap::new(),
            capable_nodes: (0, 0),
            save_task: None,
        };
        worker.reload_settings();
        worker
    }

    async fn run(&mut self, commands: &mut mpsc::Receiver<Command>, wake: &'static Notify) {
        loop {
            let now = now_secs();
            if now.saturating_sub(self.loaded_at) >= CONFIG_RELOAD_SECS
                || self.cluster_cfg.is_none()
            {
                self.reload_cluster_config().await;
                self.reload_settings();
            }
            self.step(now).await;
            let deadline = self.next_deadline(now);
            let sleep =
                tokio::time::sleep(Duration::from_secs(deadline.saturating_sub(now).max(1)));
            tokio::select! {
                _ = wake.notified() => {}
                _ = sleep => {}
                command = commands.recv() => match command {
                    Some(command) => self.handle_command(command).await,
                    None => return,
                },
                Some(event) = self.gateway_rx.recv() => {
                    self.handle_gateway_event(event).await;
                }
            }
        }
    }

    async fn reload_cluster_config(&mut self) {
        if let Ok(cfg) = crate::config::load_config().await {
            if !cfg.cluster.node_id.is_empty() {
                self.node_id = cfg.cluster.node_id.clone();
                self.ledger.local = self.node_id.clone();
            }
            self.cluster_cfg = Some(cfg);
        }
        self.loaded_at = now_secs();
    }

    /// Load `notify-config` / `notify-secrets` / `notify-openid` when their
    /// revisions moved.
    pub fn reload_settings(&mut self) {
        let config = store::load_config(&self.store);
        let secret = store::load_secret(&self.store);
        let openid = store::load_openid(&self.store);
        // Any save of the settings or the secret clears `bad_credentials`
        // (contract: "clears on the next successful READY or on a save").
        let mut saved = false;
        let mut credentials_changed = false;
        if let Ok((cfg, revision)) = config {
            if revision != self.cfg_rev {
                saved = true;
                credentials_changed |= cfg.qq != self.cfg.qq;
                self.policy = PolicyConfig::from_config(&cfg);
                self.http = build_http(&cfg);
                self.cfg = cfg;
                self.cfg_rev = revision;
            }
        }
        if let Ok((value, revision)) = secret {
            if revision != self.secret_rev {
                saved = true;
                credentials_changed = true;
                self.secret = value;
                self.secret_rev = revision;
            }
        }
        match openid {
            Ok(state) if state.record_revision != self.openid.record_revision => {
                if state.openid != self.openid.openid {
                    self.next_send_at = 0;
                }
                if !state.is_set()
                    && matches!(self.binder.state(), super::bind::BindState::Bound { .. })
                {
                    // Unbound after a bind: the bind view goes back to idle.
                    self.binder.cancel();
                }
                self.openid = state;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("QQ 通知管理员 openid 读取失败: {}", error),
        }
        if saved {
            // Stop refusing calls and retry a gateway that stopped for good.
            self.bad_credentials = false;
            self.gateway_fatal = None;
            self.next_send_at = 0;
        }
        if credentials_changed {
            // New credentials or endpoints: forget the old token and session.
            self.tokens = Arc::new(TokenCache::default());
            self.gateway = None;
        }
        // Sticky mode facts belong to one bot and one admin. Reset them only
        // when those really changed, including while this process was down;
        // never just because the process started.
        let fingerprint = self.fingerprint();
        if fingerprint != self.settings_fp {
            if !self.settings_fp.is_empty() {
                self.ledger.modes.reset_at = now_secs();
                self.dirty = true;
            }
            self.settings_fp = fingerprint;
        }
    }

    fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.cfg.qq.app_id.trim().as_bytes());
        hasher.update([0]);
        hasher.update(
            self.openid
                .openid
                .as_ref()
                .map(|id| id.expose())
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update([0]);
        hasher.update(self.secret_rev.unwrap_or(0).to_le_bytes());
        hasher
            .finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn credentials_ready(&self) -> bool {
        !self.cfg.qq.app_id.trim().is_empty() && self.secret.is_some() && !self.bad_credentials
    }

    fn capable(&self) -> bool {
        self.cfg.enabled && self.credentials_ready() && self.openid.is_set()
    }

    fn openid_value(&self) -> Option<&str> {
        self.openid.openid.as_ref().map(|id| id.expose())
    }

    /// Reason text for the status, the ledger, a log line or a response:
    /// the exact openid becomes `<set>`, then `mask_ids`.
    fn mask(&self, text: &str) -> String {
        mask_reason(text, self.openid_value())
    }

    fn sink(&self) -> Option<QqSink> {
        let secret = self.secret.clone()?;
        Some(QqSink {
            http: self.http.clone(),
            tokens: Arc::clone(&self.tokens),
            target: QqTarget {
                app_id: self.cfg.qq.app_id.trim().to_string(),
                secret,
                openid: self.openid_value().unwrap_or_default().to_string(),
                api_base: self.cfg.qq.api_base().to_string(),
                token_url: self.cfg.qq.token_url().to_string(),
            },
        })
    }

    fn next_deadline(&self, now: u64) -> u64 {
        let mut deadline = now + IDLE_TICK_SECS;
        // A node that cannot send ignores its queue for the timer (no 1 Hz
        // spin on standbys); a retry waits for next_send_at.
        let with_queue = self.authority != Authority::None && now >= self.next_send_at;
        if let Some(at) = self.tracker.next_deadline(&self.policy, with_queue) {
            deadline = deadline.min(at.max(self.next_send_at));
        }
        if self.next_send_at > now && self.tracker.queued() > 0 {
            deadline = deadline.min(self.next_send_at);
        }
        if self.binder.waiting(now) {
            deadline = deadline.min(now + 1);
        }
        deadline
    }

    pub fn digest(&self, now: u64) -> ClusterDigest {
        match &self.cluster_cfg {
            Some(cfg) => authority::read_digest(cfg, now),
            None => ClusterDigest {
                now,
                local: self.node_id.clone(),
                ..Default::default()
            },
        }
    }

    async fn step(&mut self, now: u64) {
        let mut digest = self.digest(now);
        let membership_blocked =
            digest.enabled && crate::cluster::membership::membership_block_reason().is_some();
        if self.was_membership_blocked && !membership_blocked {
            // A membership change was installed: reload the member set now,
            // not up to 30 s later.
            self.reload_cluster_config().await;
            digest = self.digest(now);
        }
        self.was_membership_blocked = membership_blocked;
        let cluster = digest.enabled && !membership_blocked;
        self.step_with(now, digest, membership_blocked).await;
        if cluster {
            self.maybe_pull(now);
        }
    }

    /// 1b: pull the openid once after joining (or starting) at a membership
    /// revision, once on becoming the writer, and every 10 minutes.
    fn maybe_pull(&mut self, now: u64) {
        let Ok((revision, _)) = super::openid_sync::current_members(&self.store) else {
            return;
        };
        let writer_local = self.writer.as_ref().is_some_and(|writer| writer.is_local);
        if let Some(mark) = next_pull(self.pull_key, revision, writer_local, now) {
            self.pull_key = Some(mark);
            super::openid_sync::spawn_pull(Arc::clone(&self.store), self.node_id.clone());
        } else if let Some(mark) = self.pull_key.as_mut() {
            // Losing the writer role updates the mark without a pull.
            mark.writer_local = writer_local;
        }
    }

    async fn step_with(&mut self, now: u64, digest: ClusterDigest, membership_blocked: bool) {
        self.apply_digest(&digest, membership_blocked, now);
        let mut seen = digest.live.clone();
        if let Some(status) = crate::webui::state::get_status_cache() {
            seen.extend(live::observations(&status));
        }
        self.channel_live(now, &seen);
        self.manage_gateway(now);
        self.deliver(now, &digest).await;
        self.publish(now, &digest);
        if self.dirty && now.saturating_sub(self.saved_at) >= STATE_SAVE_SECS {
            self.save_state(now);
        }
    }

    /// Everything that does not touch the network. Separate for tests.
    pub fn apply_digest(&mut self, digest: &ClusterDigest, membership_blocked: bool, now: u64) {
        let before = self.ledger.clone();
        for peer in &digest.peer_ledgers {
            // A peer with a fast clock must not poison the shared state.
            let mut peer = peer.clone();
            peer.clamp_future(now);
            self.ledger.join(&peer);
        }
        for node in &digest.nodes {
            if node.healthy {
                self.ever_healthy.insert(node.id.clone());
            }
        }
        self.node_names = digest
            .nodes
            .iter()
            .filter(|node| !node.name.trim().is_empty())
            .map(|node| (node.id.clone(), node.name.clone()))
            .collect();
        self.capable_nodes = if digest.enabled {
            let capable = digest
                .nodes
                .iter()
                .filter(|node| {
                    if node.id == digest.local {
                        self.capable()
                    } else {
                        node.notify_capable
                    }
                })
                .count();
            (capable, digest.nodes.len())
        } else {
            (usize::from(self.capable()), 1)
        };
        // The QQ gateway follows the sender authority (= the openid writer):
        // the owner, or the delegate while one sends for an owner without
        // the openid. One session per bot, on the node that can bind.
        self.writer = authority::openid_writer(digest, membership_blocked, self.capable())
            .map(|(node_id, is_local)| NodeRef { node_id, is_local });
        let primary =
            self.writer.as_ref().is_some_and(|writer| writer.is_local) && self.credentials_ready();
        if primary {
            self.lost_primary_at = None;
        } else if self.gateway_owner && self.lost_primary_at.is_none() {
            self.lost_primary_at = Some(now);
        }
        self.gateway_owner = primary;
        self.owner_hint = if digest.enabled {
            digest.owner.clone()
        } else {
            Some(self.node_id.clone())
        };
        self.gateway_blocker = authority::may_identify(digest).err();
        self.gateway_holder = self.gateway_owner_ref();
        self.authority = authority::decide(&DecideInput {
            digest,
            membership_blocked,
            capable: self.capable(),
            strict_single_sender: self.cfg.strict_single_sender,
            no_quorum_grace_secs: self.cfg.no_quorum_grace_secs,
            last_authority_seen: self.last_authority_seen,
        });
        if matches!(self.authority, Authority::Primary | Authority::Delegate)
            || authority::peer_has_authority(digest)
        {
            self.last_authority_seen = now;
        }
        self.binder.tick(now);

        if !self.cfg.enabled {
            self.tracker = Tracker::default();
            self.previous_owner = digest.owner.clone();
            return;
        }
        let mode = if membership_blocked
            || digest.handoff_pending
            || now.saturating_sub(self.started_at) < STARTUP_GRACE_SECS
        {
            Mode::Freeze
        } else if digest.enabled && digest.peer_count > 0 && !digest.quorum {
            Mode::Minority
        } else {
            Mode::Normal
        };
        let conditions = observer::conditions(digest, &self.ever_healthy);
        self.tracker.reconcile(now, &conditions, mode, &self.policy);
        if digest.enabled && !digest.members.is_empty() && !membership_blocked {
            // Removed through membership: not recovered, just gone.
            let members: HashSet<&str> = digest.members.iter().map(String::as_str).collect();
            self.tracker.forget_nodes(|node| !members.contains(node));
        }
        // Latch an owner change in any mode (the new owner always starts in
        // a handoff hold, i.e. Freeze) and announce it once Normal.
        if digest.enabled && digest.owner.is_some() && digest.owner != self.previous_owner {
            if let Some((old, new, reason)) =
                observer::failover(self.previous_owner.as_deref(), digest)
            {
                self.pending_failover = Some((old, new, reason, now));
            }
            self.previous_owner = digest.owner.clone();
        } else if !digest.enabled {
            self.previous_owner = digest.owner.clone();
        }
        if mode == Mode::Normal {
            if let Some((old, new, reason, seen_at)) = self.pending_failover.take() {
                if now.saturating_sub(seen_at) <= FAILOVER_LATCH_SECS {
                    self.tracker.on_event(
                        AlertEvent::new(EventKind::Failover, Phase::Fire, &new, seen_at)
                            .with_variant(&old)
                            .with_detail(&reason)
                            .with_extra(ItemExtra {
                                old_owner: Some(old.clone()),
                                live: None,
                            }),
                        &self.policy,
                    );
                }
            }
        }
        self.tracker.tick(now, &self.policy, &self.ledger);
        if self.ledger != before {
            self.dirty = true;
        }
    }

    fn manage_gateway(&mut self, now: u64) {
        self.manage_gateway_session(now);
        // Publish at once whether this node holds a session: the next
        // heartbeat carries it, so the new holder may identify.
        self.gateway_holder = self.gateway_owner_ref();
        super::shared().gateway_held = self.gateway.is_some();
    }

    fn manage_gateway_session(&mut self, now: u64) {
        let in_hysteresis = self.gateway.is_some()
            && self.credentials_ready()
            && self
                .lost_primary_at
                .is_some_and(|at| now.saturating_sub(at) < GATEWAY_HYSTERESIS_SECS);
        // `credentials_ready` includes `!bad_credentials`: rejected
        // credentials never respawn the gateway until they are saved again.
        let wanted = (self.gateway_owner || in_hysteresis)
            && self.credentials_ready()
            && ((self.cfg.enabled && self.cfg.qq.keep_online) || self.binder.waiting(now));
        if let Some(reason) = &self.gateway_fatal {
            // Stopped for good: show why until the settings change.
            self.gateway = None;
            if self.gateway_state.0 != "failed" {
                self.gateway_state = ("failed", now, Some(reason.clone()));
            }
            return;
        }
        if !wanted {
            if self.gateway.take().is_some() || self.gateway_state.0 != "off" {
                self.gateway_state = ("off", now, None);
            }
            return;
        }
        if self
            .gateway
            .as_ref()
            .is_some_and(|task| !task.0.is_finished())
        {
            return;
        }
        if let Some(peer) = &self.gateway_blocker {
            // Contract line 70: wait until the old holder has closed (its
            // heartbeat says so) or is stale/fenced. Never two sessions.
            let reason = format!("等待节点 {peer} 关闭 QQ 网关");
            if self.gateway_state.0 != "off" || self.gateway_state.2.as_deref() != Some(&reason) {
                self.gateway = None;
                self.gateway_state = ("off", now, Some(reason));
            }
            return;
        }
        let Some(secret) = self.secret.clone() else {
            return;
        };
        let params = GatewayParams {
            http: self.http.clone(),
            tokens: Arc::clone(&self.tokens),
            api_base: self.cfg.qq.api_base().to_string(),
            token_url: self.cfg.qq.token_url().to_string(),
            app_id: self.cfg.qq.app_id.trim().to_string(),
            secret,
            handshake_timeout: GatewayParams::HANDSHAKE_TIMEOUT,
            initial_backoff: GatewayParams::INITIAL_BACKOFF,
            throttle_backoff: gateway::TOKEN_THROTTLE_BACKOFF,
        };
        self.gateway_state = ("connecting", now, None);
        self.gateway = Some(GatewayTask(tokio::spawn(gateway::run(
            params,
            self.gateway_tx.clone(),
        ))));
    }

    async fn deliver(&mut self, now: u64, digest: &ClusterDigest) {
        let expired = self.tracker.expire(now);
        if !expired.is_empty() {
            if self.authority != Authority::None {
                self.ledger
                    .record_undelivered(expired.len() as u32, "超过 1 小时未能送达", now);
                self.dirty = true;
            } else if self.capable() {
                // No node could send for an hour (H2): count it and say so once.
                self.expired_no_sender =
                    self.expired_no_sender.saturating_add(expired.len() as u32);
                if !self.warned_no_sender {
                    self.warned_no_sender = true;
                    tracing::warn!(
                        "QQ 通知: {} 条通知在 1 小时内没有节点能够发送，已丢弃",
                        expired.len()
                    );
                }
            }
        }
        if self.authority == Authority::Fallback {
            // The fallback sender only speaks for itself (review §0.8).
            self.tracker
                .discard(|item| item.kind != EventKind::QuorumLost);
        }
        if self.count_unconfigured(now, digest) {
            return;
        }
        if self.authority == Authority::None || !self.capable() || now < self.next_send_at {
            return;
        }
        let batch = self.tracker.take_batch(now, &self.policy);
        if batch.is_empty() {
            return;
        }
        let label = match self.authority {
            Authority::Fallback => Some(format::LABEL_FALLBACK.to_string()),
            Authority::Delegate => Some(format::LABEL_DELEGATE.to_string()),
            _ => None,
        };
        let rendered = self.render(&batch, now, digest.owner.clone(), label);
        let outcome = match tokio::time::timeout(
            Duration::from_secs(SEND_CAP_SECS),
            self.send(&rendered, now),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => SendOutcome::Retry {
                reason: "QQ 发送超时".to_string(),
                code: None,
                trace_id: None,
            },
        };
        match outcome {
            SendOutcome::Delivered { mode, trace_id } => {
                self.tracker.delivered(&batch, now);
                for item in &batch {
                    match item.item {
                        ItemKind::Alert if item.kind != EventKind::Test => {
                            self.ledger.record_notified(&item.key, now)
                        }
                        ItemKind::Resolved { .. } => self.ledger.record_resolved(&item.key, now),
                        _ => {}
                    }
                }
                for item in std::mem::take(&mut self.held) {
                    self.ledger.record_resolved(&item.key, now);
                }
                // Structured, not string matching: every body (full or
                // minimal) carries the 未送达 count when there is one.
                if rendered.undelivered_included {
                    self.ledger.mark_undelivered_reported();
                }
                self.retry_secs = RETRY_MIN_SECS;
                self.next_send_at = 0;
                self.no_capable_node = false;
                self.outcome(true, now, Some(mode), None, None, trace_id, false);
            }
            SendOutcome::Retry {
                reason,
                code,
                trace_id,
            } => {
                self.tracker.requeue(batch);
                self.next_send_at = (now + self.retry_secs).max(self.ledger.rate_limited_until);
                self.retry_secs = (self.retry_secs * 2).min(RETRY_MAX_SECS);
                let reason = self.mask(&reason);
                self.ledger.set_reason(&reason, now);
                self.outcome(false, now, None, code, Some(reason), trace_id, false);
            }
            SendOutcome::Undeliverable {
                reason,
                code,
                trace_id,
                bad_credentials,
            } => {
                let reason = self.mask(&reason);
                tracing::warn!("QQ 通知无法送达: {} (code {:?})", reason, code);
                // Resolved items are not 「未送达」: they wait and ride along
                // with the next delivered message (spec §5).
                let (resolved, alerts): (Vec<OutItem>, Vec<OutItem>) = batch
                    .into_iter()
                    .partition(|item| matches!(item.item, ItemKind::Resolved { .. }));
                self.held.extend(resolved);
                if self.held.len() > MAX_HELD {
                    let excess = self.held.len() - MAX_HELD;
                    self.held.drain(..excess);
                }
                if !alerts.is_empty() {
                    self.ledger
                        .record_undelivered(alerts.len() as u32, &reason, now);
                }
                if bad_credentials {
                    self.bad_credentials = true;
                    self.gateway = None;
                }
                self.outcome(false, now, None, code, Some(reason), trace_id, false);
            }
        }
        self.ledger.touch(&self.node_id, now);
        self.dirty = true;
        self.save_state(now);
    }

    /// No node is fully configured (AppSecret and openid) while enabled:
    /// the writer node (execution primary, or the single node) counts the
    /// ready items as undelivered at once and raises `no_capable_node`,
    /// rather than letting them expire silently (contract line 23; Clerk's
    /// 1a gate). Resolved items are held, as for any undeliverable send.
    /// Returns true when it handled the batch.
    fn count_unconfigured(&mut self, now: u64, digest: &ClusterDigest) -> bool {
        let peer_capable = digest
            .nodes
            .iter()
            .any(|node| node.id != digest.local && node.fresh && node.notify_capable);
        let orphaned = self.cfg.enabled
            && !self.capable()
            && !peer_capable
            && self.authority == Authority::None
            && self.writer.as_ref().is_some_and(|writer| writer.is_local);
        if !orphaned {
            if self.capable() || peer_capable {
                self.no_capable_node = false;
            }
            return false;
        }
        let batch = self.tracker.take_batch(now, &self.policy);
        if batch.is_empty() {
            return false;
        }
        let (resolved, alerts): (Vec<OutItem>, Vec<OutItem>) = batch
            .into_iter()
            .partition(|item| matches!(item.item, ItemKind::Resolved { .. }));
        self.held.extend(resolved);
        if self.held.len() > MAX_HELD {
            let excess = self.held.len() - MAX_HELD;
            self.held.drain(..excess);
        }
        let alerts = alerts
            .iter()
            .filter(|item| item.kind != EventKind::Test)
            .count();
        if alerts > 0 {
            self.ledger
                .record_undelivered(alerts as u32, REASON_NO_CAPABLE_NODE, now);
            if !self.no_capable_node {
                tracing::warn!(
                    "QQ 通知: {} 条通知未送达，{}",
                    alerts,
                    REASON_NO_CAPABLE_NODE
                );
            }
            self.no_capable_node = true;
            self.dirty = true;
        }
        true
    }

    fn render_context(
        &self,
        now: u64,
        owner: Option<String>,
        label: Option<String>,
    ) -> RenderContext {
        RenderContext {
            now,
            reporter: self.node_id.clone(),
            active_owner: owner,
            label,
            undelivered: self.ledger.undelivered_pending(),
            undelivered_reason: self.ledger.last_reason.as_ref().map(|r| r.reason.clone()),
            markdown: false,
            names: self.node_names.clone(),
            held: Vec::new(),
        }
    }

    fn render(
        &self,
        batch: &[OutItem],
        now: u64,
        owner: Option<String>,
        label: Option<String>,
    ) -> Rendered {
        let mut ctx = self.render_context(now, owner, label);
        if batch.iter().all(|item| item.kind != EventKind::Test) {
            ctx.held = self.held.clone();
        }
        let text = format::render(batch, &ctx);
        let markdown = (self.cfg.style == "markdown").then(|| {
            ctx.markdown = true;
            format::render(batch, &ctx)
        });
        Rendered {
            text,
            minimal: format::minimal(batch, ctx.undelivered),
            markdown,
            // Only critical alerts may spend a 召回 (info/warning never).
            critical: batch.iter().any(|item| {
                item.severity() == super::event::Severity::Critical && item.item == ItemKind::Alert
            }),
            undelivered_included: ctx.undelivered > 0,
        }
    }

    async fn send(&mut self, rendered: &Rendered, now: u64) -> SendOutcome {
        let Some(sink) = self.sink() else {
            return SendOutcome::Undeliverable {
                reason: "通知设置不完整".to_string(),
                code: None,
                trace_id: None,
                bad_credentials: false,
            };
        };
        let mut passive = self.passive.take();
        let outcome = {
            let mut ctx = DeliveryContext {
                ledger: &mut self.ledger,
                passive: passive.as_mut(),
                now,
            };
            sink.send(rendered, &mut ctx).await
        };
        self.passive = passive;
        outcome
    }

    /// Watch-list go-live (the single call site; see `live.rs`).
    fn channel_live(&mut self, now: u64, seen: &[live::LiveObservation]) {
        self.live_now = seen.to_vec();
        let can_send =
            self.cfg.enabled && matches!(self.authority, Authority::Primary | Authority::Delegate);
        let tracker = &self.tracker;
        let events = self.live_watch.channel_live(
            now,
            self.started_at,
            &self.cfg.watch,
            seen,
            &mut self.ledger,
            can_send,
            |key| tracker.has_queued(key),
        );
        for event in events {
            self.tracker.on_event(event, &self.policy);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn outcome(
        &mut self,
        ok: bool,
        at: u64,
        mode: Option<&str>,
        code: Option<i64>,
        reason: Option<String>,
        trace_id: Option<String>,
        test: bool,
    ) {
        if !test {
            // Real sends go into the heartbeat for standbys: no reason, no ids.
            self.ledger.record_last_send(at, ok, code, mode);
        }
        self.last_outcome = Some(LastOutcome {
            ok,
            at,
            mode: mode.map(str::to_string),
            code,
            reason: reason.map(|reason| self.mask(&reason)),
            trace_id,
            node_id: self.node_id.clone(),
            test,
        });
    }

    /// Off the async thread (the store fsyncs). Writes stay in order: a new
    /// save waits for the previous one; until then the state stays dirty.
    fn save_state(&mut self, now: u64) {
        if self
            .save_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            self.dirty = true;
            return;
        }
        let state = PersistedState {
            ledger: self.ledger.clone(),
            last_outcome: self.last_outcome.clone(),
            settings: self.settings_fp.clone(),
        };
        let store = Arc::clone(&self.store);
        let write = move || {
            if let Err(error) = store::save_state(&store, &state) {
                tracing::debug!("QQ 通知状态保存失败: {}", error);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => self.save_task = Some(handle.spawn_blocking(write)),
            Err(_) => write(),
        }
        self.dirty = false;
        self.saved_at = now;
    }

    async fn handle_command(&mut self, command: Command) {
        let now = now_secs();
        match command {
            Command::Reload => {
                self.reload_settings();
                self.manage_gateway(now);
            }
            Command::Test { reply } => {
                let report = self.test_send(now).await;
                let _ = reply.send(report);
            }
            Command::StartBind {
                mode,
                replace,
                reply,
            } => {
                let _ = reply.send(self.start_bind(mode, replace, now));
                self.manage_gateway(now);
            }
            Command::CancelBind => {
                self.binder.cancel();
                self.manage_gateway(now);
            }
            Command::Event(event) => self.tracker.on_event(event, &self.policy),
        }
        let digest = self.digest(now);
        self.publish(now, &digest);
    }

    pub fn start_bind(
        &mut self,
        mode: BindMode,
        replace: bool,
        now: u64,
    ) -> Result<serde_json::Value, Refusal> {
        // Writer first (contract check order), then the bind preconditions.
        match &self.writer {
            None => return Err(Refusal::no_writer("绑定")),
            Some(writer) if !writer.is_local => {
                return Err(Refusal::not_owner(&writer.node_id, "进行绑定"))
            }
            Some(_) => {}
        }
        if !replace && self.openid.is_set() {
            return Err(Refusal::simple(
                "already_bound",
                "已绑定管理员 openid，确认后才会替换",
            ));
        }
        if self.cfg.qq.app_id.trim().is_empty() || self.secret.is_none() {
            return Err(Refusal::simple(
                "credentials_missing",
                "请先保存 AppID 和 AppSecret",
            ));
        }
        if self.bad_credentials {
            return Err(Refusal::simple(
                "bad_credentials",
                "AppID 或 AppSecret 无效，请检查后重新保存",
            ));
        }
        // O6: another node still holds a gateway session (mid-handoff): a
        // second session would kick the first, so refuse until they agree.
        // No session at all (keep_online off) is the normal case: the bind
        // window below makes `manage_gateway` open one here, briefly.
        if self
            .gateway_holder
            .as_ref()
            .is_some_and(|holder| !holder.is_local)
            || self.gateway_blocker.is_some()
            || !self.gateway_owner
        {
            return Err(Refusal::simple(
                "no_writer",
                "发送节点正在切换，请稍后再绑定",
            ));
        }
        self.binder
            .start(now, mode, super::bind::DEFAULT_WINDOW_SECS);
        Ok(self.binder.view(now, &self.node_id))
    }

    async fn test_send(&mut self, now: u64) -> TestReport {
        let mut report = TestReport {
            ok: false,
            node_id: self.node_id.clone(),
            mode: None,
            code: None,
            reason: None,
            trace_id: None,
        };
        if !self.credentials_ready() || !self.openid.is_set() {
            report.reason = Some(if self.bad_credentials {
                "AppID 或 AppSecret 无效".to_string()
            } else {
                "请先填写 AppID、AppSecret 和管理员 openid".to_string()
            });
            return report;
        }
        let item = OutItem {
            key: format!("notify.test:{}", self.node_id),
            kind: EventKind::Test,
            item: ItemKind::Alert,
            node: self.node_id.clone(),
            detail: Some("这是一条 bilistream 测试消息，收到即说明 QQ 通知可用。".to_string()),
            incident_at: now,
            queued_at: now,
            not_before: now,
            suppressed: 0,
            flapping: None,
            extra: ItemExtra::default(),
        };
        let digest = self.digest(now);
        let rendered = self.render(&[item], now, digest.owner.clone(), None);
        let outcome = tokio::time::timeout(
            Duration::from_secs(SEND_CAP_SECS),
            self.send(&rendered, now),
        )
        .await
        .unwrap_or_else(|_| SendOutcome::Retry {
            reason: "QQ 发送超时".to_string(),
            code: None,
            trace_id: None,
        });
        match outcome {
            SendOutcome::Delivered { mode, trace_id } => {
                report.ok = true;
                report.mode = Some(mode.to_string());
                report.trace_id = trace_id;
            }
            SendOutcome::Retry {
                reason,
                code,
                trace_id,
            }
            | SendOutcome::Undeliverable {
                reason,
                code,
                trace_id,
                bad_credentials: false,
            } => {
                report.reason = Some(self.mask(&reason));
                report.code = code;
                report.trace_id = trace_id;
            }
            SendOutcome::Undeliverable {
                reason,
                code,
                trace_id,
                bad_credentials: true,
            } => {
                self.bad_credentials = true;
                self.gateway = None;
                report.reason = Some(self.mask(&reason));
                report.code = code;
                report.trace_id = trace_id;
            }
        }
        self.outcome(
            report.ok,
            now,
            report.mode.as_deref(),
            report.code,
            report.reason.clone(),
            report.trace_id.clone(),
            true,
        );
        self.ledger.touch(&self.node_id, now);
        self.save_state(now);
        report
    }

    pub async fn handle_gateway_event(&mut self, event: GatewayEvent) {
        let now = now_secs();
        match event {
            GatewayEvent::Ready => {
                self.gateway_state = ("online", now, None);
                self.bad_credentials = false;
            }
            GatewayEvent::Closed {
                reason,
                fatal,
                bad_credentials,
            } => {
                let reason = self.mask(&reason);
                if bad_credentials {
                    tracing::warn!("QQ 通知: {}，等待重新保存 AppID/AppSecret", reason);
                    self.bad_credentials = true;
                }
                if fatal || bad_credentials {
                    self.gateway_fatal = Some(reason.clone());
                    self.gateway = None;
                    self.gateway_state = ("failed", now, Some(reason));
                } else {
                    self.gateway_state = ("offline", now, Some(reason));
                }
            }
            GatewayEvent::ActiveMessages { openid, .. }
                if self.openid_value().is_none() || openid.as_deref() != self.openid_value() =>
            {
                tracing::debug!("QQ 网关收到非管理员的主动消息开关事件，已忽略");
            }
            GatewayEvent::ActiveMessages { accepted, at, .. } => {
                if accepted {
                    self.ledger.modes.admin_accepted_at = at.max(1);
                } else {
                    self.ledger.modes.admin_rejected_at = at.max(1);
                }
                self.dirty = true;
            }
            GatewayEvent::C2c {
                openid,
                msg_id,
                content,
                at,
            } => match self.binder.on_message(&openid, &content, at) {
                BindDecision::Bind { openid, via } => {
                    self.finish_bind(&openid, via, Some(&msg_id), None, at)
                        .await;
                }
                BindDecision::Ignore => {
                    if !openid.is_empty() && Some(openid.as_str()) == self.openid_value() {
                        // The admin wrote to the bot: a passive reply window
                        // opens and the wakeup cycle restarts.
                        self.passive = Some(PassiveTarget {
                            msg_id: msg_id.clone(),
                            received_at: at,
                            replies_used: 0,
                        });
                        self.ledger.record_admin_message(at);
                        self.dirty = true;
                        // Only a read-only command gets an answer.
                        if let Some(cmd) = commands::parse(&content) {
                            self.answer_command(cmd, now).await;
                        }
                    } else {
                        tracing::debug!("QQ 网关收到绑定窗口外的消息，已忽略");
                    }
                }
            },
            GatewayEvent::FriendAdd {
                openid,
                event_id,
                at,
            } => match self.binder.on_friend_add(&openid, at) {
                BindDecision::Bind { openid, via } => {
                    self.finish_bind(&openid, via, None, event_id.as_deref(), at)
                        .await;
                }
                BindDecision::Ignore => {
                    tracing::debug!("QQ 网关收到绑定窗口外的加好友事件，已忽略")
                }
            },
        }
        let digest = self.digest(now);
        self.publish(now, &digest);
    }

    /// Answer a read-only command with one passive reply. Rate-limited;
    /// reads state, never changes it (besides the reply slot it uses).
    async fn answer_command(&mut self, cmd: commands::Cmd, now: u64) {
        if !self.cfg.enabled || !self.cfg.commands_enabled || !self.gateway_owner {
            tracing::debug!("QQ 指令未启用或本节点不是网关节点，已忽略");
            return;
        }
        if !self.limiter.allow(now) {
            tracing::debug!("QQ 指令回复过于频繁，已忽略");
            return;
        }
        let Some(target) = self.passive.clone().filter(|target| target.usable(now)) else {
            return;
        };
        let digest = self.digest(now);
        let ctx = self.render_context(now, digest.owner.clone(), None);
        let watched: HashMap<String, String> = self
            .cfg
            .watch
            .channels
            .iter()
            .map(|channel| (channel.ident(), channel.display().to_string()))
            .collect();
        let mut live_names: Vec<(String, Option<String>)> = Vec::new();
        for obs in &self.live_now {
            let name = watched
                .get(&obs.ident())
                .cloned()
                .unwrap_or_else(|| obs.channel_id.clone());
            if !live_names.iter().any(|(existing, _)| *existing == name) {
                live_names.push((name, obs.title.clone()));
            }
        }
        let open = self
            .tracker
            .open_entries()
            .into_iter()
            .map(|(kind, node, since, severity)| commands::OpenEntry {
                kind,
                node,
                since,
                severity,
            })
            .collect();
        let text = commands::reply(
            cmd,
            &commands::View {
                now,
                ctx: &ctx,
                digest: &digest,
                ledger: &self.ledger,
                open,
                passive: Some(&target),
                live: live_names,
                gateway_online: self.gateway_state.0 == "online",
                membership_paused: self.was_membership_blocked,
            },
        );
        let Some(sink) = self.sink() else {
            return;
        };
        let token =
            match tokio::time::timeout(Duration::from_secs(SEND_CAP_SECS), sink.token()).await {
                Ok(Ok(token)) => token,
                _ => return,
            };
        let seq = target.replies_used + 1;
        let result = api::send_c2c(
            &self.http,
            self.cfg.qq.api_base(),
            &token,
            &C2cMessage {
                openid: self.openid_value().unwrap_or_default(),
                content: &text,
                markdown: false,
                mode: QqMode::Passive,
                msg_id: Some(&target.msg_id),
                event_id: None,
                msg_seq: seq,
            },
        )
        .await;
        if result.http_status != 0 {
            if let Some(passive) = self.passive.as_mut().filter(|p| p.msg_id == target.msg_id) {
                passive.replies_used = seq;
            }
        }
        if !result.ok {
            tracing::debug!(
                "QQ 指令回复未送达: http={} code={:?}",
                result.http_status,
                result.code
            );
        }
    }

    async fn finish_bind(
        &mut self,
        openid: &str,
        via: BindVia,
        msg_id: Option<&str>,
        event_id: Option<&str>,
        at: u64,
    ) {
        // Save first, sealed in `notify-openid`: the binding must survive a
        // failed reply. The pairing itself is the newest intent, so it takes
        // the next openid_revision without a revision check.
        let write = OpenidWrite {
            change: OpenidChange::Set {
                value: zeroize::Zeroizing::new(openid.to_string()),
                via: match via {
                    BindVia::Message => "message",
                    BindVia::FriendAdd => "friend_add",
                },
            },
            expected: None,
            writer: self.node_id.clone(),
            now: at,
        };
        match store::write_openid(&self.store, write) {
            Ok(revision) => {
                tracing::info!(
                    "QQ 通知管理员 openid 已绑定 (openid=<set>, revision {})",
                    revision
                );
                self.reload_settings();
                // 1b: every current member gets the new value.
                super::openid_sync::spawn_push(Arc::clone(&self.store), self.node_id.clone());
            }
            Err(error) => tracing::warn!("QQ 通知 openid 保存失败: {}", error),
        }
        if via == BindVia::Message {
            self.ledger.record_admin_message(at);
            self.dirty = true;
        }
        let Some(sink) = self.sink() else {
            return;
        };
        let token = match sink.token().await {
            Ok(token) => token,
            Err(error) => {
                tracing::debug!("绑定成功回复取消: {}", error.reason);
                return;
            }
        };
        let result = api::send_c2c(
            &self.http,
            self.cfg.qq.api_base(),
            &token,
            &C2cMessage {
                openid,
                content: "绑定成功",
                markdown: false,
                mode: QqMode::Passive,
                msg_id,
                event_id,
                msg_seq: 1,
            },
        )
        .await;
        if let Some(msg_id) = msg_id {
            self.passive = Some(PassiveTarget {
                msg_id: msg_id.to_string(),
                received_at: at,
                replies_used: 1,
            });
        }
        if !result.ok {
            tracing::debug!(
                "绑定成功回复未送达: http={} code={:?}",
                result.http_status,
                result.code
            );
        }
    }

    fn publish(&self, now: u64, digest: &ClusterDigest) {
        let status = self.status(now, digest);
        self.publish_status(status);
    }

    /// The `/api/notify/status` body this worker owns (no openid, ever).
    pub fn status(&self, now: u64, digest: &ClusterDigest) -> serde_json::Value {
        let ledger = &self.ledger;
        serde_json::json!({
            "worker": "running",
            "node_id": self.node_id,
            "enabled": self.cfg.enabled,
            "capable": self.capable(),
            "credentials_ready": self.credentials_ready(),
            "bad_credentials": self.bad_credentials,
            "openid_set": self.openid.is_set(),
            "openid_bound_at": self.openid.bound_at,
            "openid_revision": self.openid.revision,
            "authority": self.authority.as_str(),
            "openid_writer": self.writer,
            "gateway_owner": self.gateway_holder,
            "owner_node": self.owner_hint,
            "cluster_enabled": digest.enabled,
            "gateway": {
                "state": self.gateway_state.0,
                "since": self.gateway_state.1,
                "reason": self.gateway_state.2,
            },
            "bind": self.binder.view(now, &self.node_id),
            "open_incidents": self.tracker.open_incidents(),
            "queued": self.tracker.queued(),
            "next_retry_at": (self.next_send_at > now).then_some(self.next_send_at),
            "local_last_send": self.last_outcome,
            // Deprecated duplicate of `local_last_send` (panel batch 2 drops it).
            "last_outcome": self.last_outcome,
            "quota": {
                "active_day": ledger.active_today(now),
                "active_day_limit": super::ledger::ACTIVE_PER_DAY,
                "rate_limited_until": (ledger.rate_limited_until > now).then_some(ledger.rate_limited_until),
                "wakeups_left": ledger.wakeups_left(now),
                "admin_last_message_at": ledger.wakeup.admin_last_c2c_at,
                "passive_window": self.passive.as_ref().is_some_and(|p| p.usable(now)),
            },
            "modes": {
                "active_unavailable": ledger.modes.active_unavailable(now),
                "admin_rejected": ledger.modes.admin_rejected(),
                "markdown_unsupported": ledger.modes.markdown_unsupported(),
            },
            "undelivered": ledger.undelivered_pending(),
            "last_reason": ledger.last_reason,
            "held_resolved": self.held.len(),
            "expired_no_sender": self.expired_no_sender,
            "warning": self.no_capable_node.then_some(WARNING_NO_CAPABLE_NODE),
            "capable_nodes": self.capable_nodes.0,
            "cluster_nodes": self.capable_nodes.1,
            "commands_enabled": self.cfg.commands_enabled,
            "watch": {
                "enabled": self.cfg.watch.enabled,
                "channels": self.cfg.watch.channels.len(),
                "live": self.live_now.iter().map(|obs| obs.ident()).collect::<Vec<_>>(),
            },
        })
    }

    fn publish_status(&self, status: serde_json::Value) {
        let signature = format!(
            "{}|{:?}|{}|{}|{}|{:?}|{:?}|{}|{}",
            self.gateway_state.0,
            self.binder.state(),
            self.authority.as_str(),
            self.capable(),
            self.ledger.undelivered_pending(),
            self.last_outcome.as_ref().map(|o| (o.ok, o.at)),
            self.writer,
            self.openid.revision,
            self.bad_credentials,
        );
        {
            let mut shared = super::shared();
            shared.ledger = self.ledger.clone();
            shared.capable = self.capable();
            shared.authority = matches!(self.authority, Authority::Primary | Authority::Delegate);
            shared.status = status;
            shared.writer = self.writer.clone();
            shared.gateway_held = self.gateway.is_some();
            shared.appsecret_set = self.secret.is_some();
            shared.appid_set = !self.cfg.qq.app_id.trim().is_empty();
            shared.openid_set = self.openid.is_set();
        }
        let mut last = self.signature.lock().unwrap_or_else(|e| e.into_inner());
        if *last != signature {
            *last = signature;
            #[cfg(not(test))]
            crate::webui::events::publish("status");
        }
    }
}

impl Worker {
    /// `{node_id, is_local}` of the node that really holds the QQ gateway
    /// session (`/status.gateway_owner`, informational): this node while its
    /// session task runs, else the peer whose heartbeat says it holds one
    /// (`authority::may_identify`), else null (e.g. the handoff gap, or
    /// `keep_online` off between bind windows).
    fn gateway_owner_ref(&self) -> Option<NodeRef> {
        if self.gateway.is_some() {
            return Some(NodeRef {
                node_id: self.node_id.clone(),
                is_local: true,
            });
        }
        self.gateway_blocker.clone().map(|node_id| NodeRef {
            node_id,
            is_local: false,
        })
    }
}

fn build_http(cfg: &NotifyConfig) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15));
    if let Some(proxy) = cfg
        .qq
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        match reqwest::Proxy::all(proxy) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(_) => tracing::warn!("QQ 通知代理地址无效，已直连"),
        }
    }
    builder.build().unwrap_or_default()
}

#[cfg(test)]
#[path = "worker_tests.rs"]
mod tests;
