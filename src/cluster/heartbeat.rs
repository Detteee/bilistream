//! Heartbeat worker, outbound pings, and unreachable-peer accounting.

use super::election::{is_stale, last_seen_is_stale};
use super::fencing::{
    clear_local_stream, collect_local_snapshot, local_has_fresh_quorum,
    local_monitoring_block_reason, HEARTBEAT_FAILURE_THRESHOLD,
};
use super::state::{cluster_heartbeat_timeout, cluster_state_read, cluster_state_write, now_secs};
use super::status::{
    canonicalize_node_membership, compute_cluster_status_with_version, current_active_owner,
    empty_node, heartbeat_response_is_valid, merge_direct_peer_status, update_node,
};
use super::sync::{
    adopt_auto_failover_from_peer_view, retry_unconfirmed_demotion, HandoffObserver,
    HandoffProgress, HandoffStage, NodeModeError, SourceStopEvidence,
};
use super::types::*;
use super::version::monitored_config_version;
use crate::config::Config;
use crate::logging::{human_duration, safe_diagnostic, safe_text};
use crate::plugins::{
    enable_danmaku_commands, is_danmaku_running, is_ffmpeg_running, set_manual_restart,
    stop_danmaku, stop_ffmpeg,
};
use axum::body::Bytes;
use futures_util::future::join_all;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) static AUTO_TRANSITION_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

const HANDOFF_REPORT_INTERVAL: Duration = Duration::from_secs(60);
static NEXT_HANDOFF_LOG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct HandoffView {
    owner: Option<String>,
    block: Option<String>,
}

impl HandoffView {
    fn current(cfg: &Config) -> Self {
        // Snapshot before taking the reporting lock; formatting never reads
        // configuration, cluster state, or the execution gate.
        Self {
            owner: current_active_owner(),
            block: local_monitoring_block_reason(cfg),
        }
    }

    fn gate(&self, compact: bool) -> String {
        self.block.as_ref().map_or_else(
            || {
                if compact {
                    "允许"
                } else {
                    "允许监控/推流"
                }
                .to_string()
            },
            |reason| {
                let label = if compact {
                    match reason.as_str() {
                        "集群节点交接尚未确认，暂停本节点执行" => "交接待确认",
                        "集群心跳多数派已丢失" => "缺少心跳多数派",
                        "集群活跃节点存在分歧或尚未获得多数节点确认" => {
                            "节点选择未达成一致"
                        }
                        _ => reason,
                    }
                } else {
                    reason
                };
                format!("受阻（{}）", safe_text(label, 120))
            },
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HandoffReportPhase {
    Deferred(&'static str),
    Failed(HandoffStage, &'static str, SourceStopEvidence),
}

struct HandoffEpisode {
    source: String,
    started: Instant,
    attempts: u64,
    failed_requests: u64,
    last_report: Option<Instant>,
    last_report_state: Option<(HandoffReportPhase, HandoffView)>,
    stage: HandoffStage,
    source_stop: SourceStopEvidence,
    error: Option<(&'static str, String)>,
    http_statuses: Vec<u16>,
    config_fallback: Option<String>,
    reported_config_fallback: bool,
    reported_source_step: Option<SourceStopEvidence>,
    reported_request_failure: bool,
    deferred: bool,
    in_flight: bool,
    stopped: Option<String>,
    finished: bool,
}

/// One current automatic assignment, owned by the heartbeat worker. A running
/// attempt can retain its handle after an assignment change, solely to report
/// its actual result. No reporting flag participates in scheduling or fencing.
struct AutomaticHandoffLog {
    id: u64,
    node: String,
    target: String,
    episode: Mutex<HandoffEpisode>,
}

impl AutomaticHandoffLog {
    #[cfg(test)]
    fn new(node: &str, source: &str, target: &str, view: &HandoffView, now: Instant) -> Self {
        Self::with_reason(node, source, target, "选举改选", view, now)
    }

    fn with_reason(
        node: &str,
        source: &str,
        target: &str,
        reason: &str,
        view: &HandoffView,
        now: Instant,
    ) -> Self {
        let report = Self {
            id: NEXT_HANDOFF_LOG_ID.fetch_add(1, Ordering::Relaxed),
            node: node.to_string(),
            target: target.to_string(),
            episode: Mutex::new(HandoffEpisode {
                source: source.to_string(),
                started: now,
                attempts: 0,
                failed_requests: 0,
                last_report: None,
                last_report_state: None,
                stage: HandoffStage::Prepare,
                source_stop: SourceStopEvidence::Pending,
                error: None,
                http_statuses: Vec::new(),
                config_fallback: None,
                reported_config_fallback: false,
                reported_source_step: None,
                reported_request_failure: false,
                deferred: false,
                in_flight: false,
                stopped: None,
                finished: false,
            }),
        };
        {
            let state = report.lock();
            tracing::info!(
                "{}",
                report.milestone(
                    &state,
                    &format!("开始；{}", safe_text(reason, 64)),
                    view,
                    now
                )
            );
        }
        report
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HandoffEpisode> {
        self.episode.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn matches(&self, source: &str, target: &str) -> bool {
        let state = self.lock();
        !state.finished && state.source == source && self.target == target
    }

    fn line(
        &self,
        state: &HandoffEpisode,
        action: &str,
        view: &HandoffView,
        now: Instant,
    ) -> String {
        let elapsed = now.saturating_duration_since(state.started).as_secs();
        format!(
            "{}；轮次={} 模式更新失败={} 耗时={} 阶段={}；源停播={}；所选={}；本机执行={}",
            self.prefix(state, action),
            state.attempts,
            state.failed_requests,
            human_duration(elapsed.min(i64::MAX as u64) as i64),
            state.stage.label(),
            state.source_stop.label(),
            safe_text(view.owner.as_deref().unwrap_or("无"), 64),
            view.gate(false),
        )
    }

    fn prefix(&self, state: &HandoffEpisode, action: &str) -> String {
        format!(
            "{} 自动交接 {}→{} #{} {}",
            crate::logging::node_tag(&self.node),
            safe_text(&state.source, 64),
            safe_text(&self.target, 64),
            self.id,
            action,
        )
    }

    fn milestone(
        &self,
        state: &HandoffEpisode,
        action: &str,
        view: &HandoffView,
        now: Instant,
    ) -> String {
        let mut line = self.prefix(state, action);
        if state.attempts > 0 {
            let elapsed = now.saturating_duration_since(state.started).as_secs();
            line.push_str(&format!(
                "；{}轮/{}",
                state.attempts,
                human_duration(elapsed.min(i64::MAX as u64) as i64)
            ));
        }
        line.push_str(&format!(
            "；所选={} 本机执行={}",
            safe_text(view.owner.as_deref().unwrap_or("无"), 64),
            view.gate(true)
        ));
        if state.failed_requests > 0 {
            line.push_str(&format!("；模式更新失败={}", state.failed_requests));
        }
        if matches!(
            state.source_stop,
            SourceStopEvidence::Unconfirmed | SourceStopEvidence::MembershipProof
        ) {
            line.push_str(&format!("；源停播={}", state.source_stop.label()));
        }
        line
    }

    fn warn_if_due(
        &self,
        state: &mut HandoffEpisode,
        phase: HandoffReportPhase,
        action: &str,
        detail: &str,
        view: &HandoffView,
        now: Instant,
    ) {
        let signature = (phase, view.clone());
        let changed = state.last_report_state.as_ref() != Some(&signature);
        let due = state
            .last_report
            .is_none_or(|last| now.saturating_duration_since(last) >= HANDOFF_REPORT_INTERVAL);
        if changed || due {
            state.last_report = Some(now);
            state.last_report_state = Some(signature);
            tracing::warn!("{}{}", self.line(state, action, view, now), detail);
        }
    }

    fn defer(&self, reason: &'static str, view: &HandoffView, now: Instant) {
        let mut state = self.lock();
        if state.finished {
            return;
        }
        state.deferred = true;
        self.warn_if_due(
            &mut state,
            HandoffReportPhase::Deferred(reason),
            &format!("暂缓：{reason}；等待后续心跳条件满足"),
            "",
            view,
            now,
        );
    }

    fn begin_attempt(&self, view: &HandoffView, now: Instant) {
        let mut state = self.lock();
        state.attempts = state.attempts.saturating_add(1);
        state.in_flight = true;
        state.stage = HandoffStage::Prepare;
        state.source_stop = SourceStopEvidence::Pending;
        state.error = None;
        state.config_fallback = None;
        if state.deferred {
            state.deferred = false;
            tracing::info!(
                "{}",
                self.line(&state, "已选出目标且多数派条件已满足，继续交接", view, now)
            );
        }
    }

    fn observe(&self, progress: HandoffProgress, view: &HandoffView, now: Instant) {
        let mut state = self.lock();
        match progress {
            HandoffProgress::Source(source) => {
                if state.source != source {
                    let previous = std::mem::replace(&mut state.source, source);
                    state.last_report = None;
                    state.last_report_state = None;
                    state.reported_config_fallback = false;
                    state.reported_source_step = None;
                    state.reported_request_failure = false;
                    state.http_statuses.clear();
                    tracing::info!(
                        "{}；原请求源={}",
                        self.line(&state, "待交接源已变更，按实际源继续校验", view, now),
                        safe_text(&previous, 64)
                    );
                }
            }
            HandoffProgress::Stage(stage) => {
                state.stage = stage;
                state.error = None;
            }
            HandoffProgress::ConfigFallback(error) => {
                state.config_fallback = Some(safe_diagnostic(&error));
                if !state.reported_config_fallback {
                    state.reported_config_fallback = true;
                    tracing::warn!(
                        "{}；原因={}",
                        self.line(
                            &state,
                            "源配置读取失败，沿用同步副本和缓存监控状态",
                            view,
                            now
                        ),
                        state.config_fallback.as_deref().unwrap_or_default()
                    );
                }
            }
            HandoffProgress::SourceStopped(evidence) => {
                state.source_stop = evidence;
                if state.reported_source_step != Some(evidence) {
                    state.reported_source_step = Some(evidence);
                    let line = self.line(&state, "源停播校验已结束，继续启用目标", view, now);
                    if evidence == SourceStopEvidence::Unconfirmed {
                        tracing::warn!("{line}");
                    } else {
                        tracing::info!("{line}");
                    }
                }
                // This records the current handoff phase evidence. The separate
                // owed-demotion result remains in retry_unconfirmed_demotion.
            }
            HandoffProgress::FailedRequest(error) => {
                state.failed_requests = state.failed_requests.saturating_add(1);
                Self::remember_error(&mut state, &error);
                if !state.reported_request_failure {
                    state.reported_request_failure = true;
                    self.report_failure(&mut state, "模式更新失败，按既有策略重试", view, now);
                }
            }
            HandoffProgress::FailedStage(error) => Self::remember_error(&mut state, &error),
        }
    }

    fn remember_error(state: &mut HandoffEpisode, error: &NodeModeError) {
        let class = match error {
            NodeModeError::Unreachable(_) => "unreachable",
            NodeModeError::Refused(_) => "refused",
        };
        let diagnostic = safe_diagnostic(&error.to_string());
        // Keep a bounded set of status codes, not a URL/string-based suppression
        // key. Alternating 530/502 remains one episode and one reporting clock.
        for part in diagnostic.split("HTTP ").skip(1) {
            if let Ok(status) = part.chars().take(3).collect::<String>().parse::<u16>() {
                if (100..=599).contains(&status)
                    && !state.http_statuses.contains(&status)
                    && state.http_statuses.len() < 4
                {
                    state.http_statuses.push(status);
                }
            }
        }
        state.error = Some((class, diagnostic));
    }

    fn report_failure(
        &self,
        state: &mut HandoffEpisode,
        outcome: &str,
        view: &HandoffView,
        now: Instant,
    ) {
        let (class, error) = state
            .error
            .as_ref()
            .map(|(class, error)| (*class, error.as_str()))
            .unwrap_or(("handoff_check", "未提供诊断文本"));
        let statuses = state
            .http_statuses
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let detail = format!("；类别={class} HTTP=[{statuses}]；原因={error}");
        let phase = HandoffReportPhase::Failed(state.stage, class, state.source_stop);
        self.warn_if_due(state, phase, outcome, &detail, view, now);
    }

    fn finish(&self, result: &Result<(), String>, view: &HandoffView, now: Instant) {
        let mut state = self.lock();
        state.in_flight = false;
        if result.is_ok() {
            state.finished = true;
            let fallback = if state.config_fallback.is_some() {
                "；配置使用本机同步副本"
            } else {
                ""
            };
            tracing::info!(
                "{}{}",
                self.milestone(&state, "本轮交接已完成", view, now),
                fallback
            );
        } else if let Some(reason) = &state.stopped {
            let action = format!("本轮未完成，后续自动重试已停止：{reason}");
            state.finished = true;
            tracing::info!("{}", self.line(&state, &action, view, now));
        } else if view
            .owner
            .as_deref()
            .is_some_and(|owner| owner != state.source && owner != self.target)
        {
            state.finished = true;
            tracing::info!(
                "{}",
                self.line(&state, "已被新的节点选择取代，本轮未完成", view, now)
            );
        } else {
            if state.error.is_none() {
                state.error = Some((
                    "handoff_check",
                    safe_diagnostic(result.as_ref().err().map(String::as_str).unwrap_or("")),
                ));
            }
            self.report_failure(&mut state, "本轮未完成，等待自动重试", view, now);
        }
    }

    fn stop_retries(&self, reason: &str, view: &HandoffView, now: Instant) {
        let mut state = self.lock();
        if state.finished || state.stopped.is_some() {
            return;
        }
        let reason = safe_text(reason, 120);
        state.stopped = Some(reason.clone());
        state.finished = !state.in_flight;
        let action = if state.in_flight {
            format!("停止后续自动重试：{reason}；已有批次仍待实际结果")
        } else {
            format!("后续自动交接已取消：{reason}")
        };
        tracing::info!("{}", self.line(&state, &action, view, now));
    }
}

fn stop_handoff_report(current: &mut Option<Arc<AutomaticHandoffLog>>, cfg: &Config, reason: &str) {
    if let Some(report) = current.take() {
        report.stop_retries(reason, &HandoffView::current(cfg), Instant::now());
    }
}

pub struct ClusterWorker {
    heartbeat: tokio::task::JoinHandle<()>,
    self_check: tokio::task::JoinHandle<()>,
    external_api: tokio::task::JoinHandle<()>,
    yt_index: tokio::task::JoinHandle<()>,
}

impl Drop for ClusterWorker {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.self_check.abort();
        self.external_api.abort();
        self.yt_index.abort();
    }
}

pub fn start_cluster_worker() -> ClusterWorker {
    super::sync::mark_process_started();
    super::fencing::start_execution_fence_watchdog();
    let heartbeat = tokio::spawn(async {
        let mut handoff_report = None;
        loop {
            let cycle_started = Instant::now();
            let cfg = match crate::config::load_config()
                .await
                .map_err(|e| e.to_string())
            {
                Ok(cfg) => cfg,
                Err(error) => {
                    tracing::debug!("Cluster worker skipped config load: {}", error);
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    continue;
                }
            };

            if !cfg.cluster.enabled {
                stop_handoff_report(&mut handoff_report, &cfg, "集群已关闭");
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }

            let config_version = monitored_config_version(&cfg);
            let local = collect_local_snapshot(&cfg, config_version.clone()).await;
            let previous_owner = current_active_owner();
            update_node(local.clone(), &cfg.cluster.node_id);
            send_heartbeats(&cfg, local).await;
            let status = compute_cluster_status_with_version(&cfg, config_version);
            schedule_auto_owner_transition(&cfg, previous_owner, &status, &mut handoff_report);
            schedule_unconfirmed_demotion(&cfg);
            let block_reason = local_monitoring_block_reason(&cfg);
            if let Some(reason) = block_reason {
                if is_ffmpeg_running().await {
                    tracing::warn!("{}，停止本节点 ffmpeg 推流", reason);
                    set_manual_restart();
                    clear_local_stream();
                    stop_ffmpeg().await;
                }
                enable_danmaku_commands(false);
                if is_danmaku_running() {
                    tracing::info!("{}，停止本节点弹幕客户端", reason);
                    stop_danmaku().await;
                }
            }

            tokio::time::sleep(heartbeat_cycle_delay(
                heartbeat_sleep_duration(&cfg),
                cycle_started.elapsed(),
            ))
            .await;
        }
    });
    // Self-check latency cannot delay heartbeats to healthy peers, and neither
    // can YouTube index fetches or the owner's Google calls.
    ClusterWorker {
        heartbeat,
        self_check: tokio::spawn(super::self_check::run_self_checks()),
        external_api: tokio::spawn(super::external_api::run_external_api_checks()),
        yt_index: tokio::spawn(super::yt_index::run_yt_index()),
    }
}

pub(crate) fn heartbeat_cycle_delay(period: Duration, elapsed: Duration) -> Duration {
    period.saturating_sub(elapsed)
}

pub(crate) fn heartbeat_sleep_duration(cfg: &Config) -> Duration {
    let base = Duration::from_secs(cfg.cluster.heartbeat_interval_secs.max(1));
    let jitter_ms = stable_node_jitter_ms(&cfg.cluster.node_id, 1_000);
    base + Duration::from_millis(jitter_ms)
}

pub(crate) fn stable_node_jitter_ms(node_id: &str, max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    node_id.hash(&mut hasher);
    hasher.finish() % max_ms
}

pub(crate) fn current_forced_owner() -> Option<String> {
    cluster_state_read().forced_owner.clone()
}

fn schedule_auto_owner_transition(
    cfg: &Config,
    previous_owner: Option<String>,
    status: &ClusterStatus,
    handoff_report: &mut Option<Arc<AutomaticHandoffLog>>,
) {
    if !cfg.cluster.auto_failover {
        stop_handoff_report(handoff_report, cfg, "自动切换已关闭");
        return;
    }
    let previous_owner = cluster_state_read()
        .pending_handoff_source
        .clone()
        .or(previous_owner);
    let Some(previous_owner) = previous_owner else {
        if handoff_report
            .as_ref()
            .is_some_and(|report| !report.lock().in_flight)
        {
            stop_handoff_report(handoff_report, cfg, "已无待交接源节点");
        }
        return;
    };
    let Some(new_owner) = status.active_owner.as_deref() else {
        if let Some(report) = handoff_report {
            report.defer(
                "尚未选出目标节点",
                &HandoffView::current(cfg),
                Instant::now(),
            );
        }
        return;
    };
    if previous_owner == new_owner {
        if handoff_report
            .as_ref()
            .is_some_and(|report| !report.lock().in_flight)
        {
            stop_handoff_report(handoff_report, cfg, "当前源与所选目标相同");
        }
        return;
    }
    if current_forced_owner().is_some() {
        stop_handoff_report(handoff_report, cfg, "节点选择已由手动切换接管");
        return;
    }
    if cfg.cluster.node_id != new_owner {
        stop_handoff_report(
            handoff_report,
            cfg,
            &format!("已改选节点 {}", safe_text(new_owner, 64)),
        );
        return;
    }
    let view = HandoffView::current(cfg);
    let now = Instant::now();
    if handoff_report
        .as_ref()
        .is_some_and(|report| !report.matches(&previous_owner, new_owner))
    {
        stop_handoff_report(handoff_report, cfg, "交接源或目标已变更");
    }
    let report = handoff_report.get_or_insert_with(|| {
        let reason = match status
            .nodes
            .iter()
            .find(|node| node.node_id == previous_owner)
        {
            Some(node) if node.draining => "源节点排空".to_string(),
            Some(node) if !node.health.healthy => {
                format!("源节点状态={}", safe_text(&node.health.reason, 48))
            }
            None => "当前状态中已无源节点".to_string(),
            _ => "选举改选".to_string(),
        };
        Arc::new(AutomaticHandoffLog::with_reason(
            &cfg.cluster.node_id,
            &previous_owner,
            new_owner,
            &reason,
            &view,
            now,
        ))
    });
    if !local_has_fresh_quorum(cfg) {
        report.defer("缺少新鲜心跳多数派", &view, now);
        return;
    }

    if AUTO_TRANSITION_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::debug!("Cluster owner transition already in progress");
        return;
    }

    let preserve_source_drain = status
        .nodes
        .iter()
        .find(|node| node.node_id == previous_owner)
        .is_some_and(|node| node.draining);
    let cfg = cfg.clone();
    let status = status.clone();
    let new_owner = new_owner.to_string();
    let report = Arc::clone(report);
    report.begin_attempt(&view, now);
    tokio::spawn(async move {
        let progress_report = Arc::clone(&report);
        let progress_cfg = cfg.clone();
        let observer: HandoffObserver = Arc::new(move |progress| {
            progress_report.observe(
                progress,
                &HandoffView::current(&progress_cfg),
                Instant::now(),
            );
        });
        let result = super::sync::finalize_cluster_node_switch_observed(
            &cfg,
            &status,
            &previous_owner,
            &new_owner,
            preserve_source_drain,
            true,
            Some(observer),
        )
        .await;
        report.finish(&result, &HandoffView::current(&cfg), Instant::now());
        AUTO_TRANSITION_IN_FLIGHT.store(false, Ordering::Release);
    });
}

static DEMOTION_RETRY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Off the heartbeat path: a timeout to a dead source must not delay
/// heartbeats to healthy peers.
fn schedule_unconfirmed_demotion(cfg: &Config) {
    if cluster_state_read().unconfirmed_demotion.is_none()
        || DEMOTION_RETRY_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        retry_unconfirmed_demotion(&cfg).await;
        DEMOTION_RETRY_IN_FLIGHT.store(false, Ordering::Release);
    });
}

pub(crate) fn record_heartbeat(cfg: &Config, mut node: ClusterNodeSnapshot) -> bool {
    if !cfg.cluster.enabled {
        tracing::debug!("Ignored cluster heartbeat while cluster mode is disabled");
        return false;
    }
    if node.node_id == cfg.cluster.node_id {
        tracing::debug!(
            "Ignored heartbeat claiming local node identity {}",
            node.node_id
        );
        return false;
    }
    if !canonicalize_node_membership(&mut node, cfg) {
        tracing::debug!("Ignored heartbeat from unknown node {}", node.node_id);
        return false;
    }
    node.last_seen = Some(now_secs());
    node.is_local = false;
    let mut state = cluster_state_write();
    state.heartbeat_failures.remove(&node.node_id);
    state.nodes.insert(node.node_id.clone(), node);
    true
}

pub(crate) async fn send_heartbeats(cfg: &Config, local: ClusterNodeSnapshot) {
    let request = ClusterHeartbeatRequest { node: local };
    let body = match encode_heartbeat(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!("Could not encode local cluster heartbeat: {error}");
            return;
        }
    };
    // Fan out concurrently: one slow/dead peer must not delay heartbeats to the
    // others, otherwise healthy peers may see this node as stale.
    let tasks = cfg
        .cluster
        .peers
        .iter()
        .filter(|peer| peer.node_id != cfg.cluster.node_id)
        .map(|peer| send_heartbeat_to_peer(cfg, peer, body.clone()));
    join_all(tasks).await;
}

fn encode_heartbeat(request: &ClusterHeartbeatRequest) -> Result<Bytes, serde_json::Error> {
    // Every peer receives identical bytes. Encoding once also avoids one
    // payload-sized allocation per peer; Bytes clones only share ownership.
    serde_json::to_vec(request).map(Bytes::from)
}

pub(crate) async fn send_heartbeat_to_peer(
    cfg: &Config,
    peer: &crate::config::ClusterPeer,
    body: Bytes,
) {
    let result = super::peer_call::send_ordinary(
        cfg,
        &peer.node_id,
        super::peer_call::routes::HEARTBEAT,
        body.to_vec(),
        cluster_heartbeat_timeout(cfg),
    )
    .await;

    match result {
        Ok(response) if !(200..300).contains(&response.status) => {
            tracing::debug!(
                "Cluster heartbeat failed for {}: HTTP {}",
                peer.node_id,
                response.status
            );
            mark_peer_unreachable(&peer.node_id, cfg);
        }
        Ok(response) => {
            match serde_json::from_slice::<PeerApiResponse<ClusterStatus>>(&response.body) {
                Ok(envelope) if envelope.success => {
                    if let Some(status) = envelope.data {
                        if heartbeat_response_is_valid(&status, &peer.node_id) {
                            let peer_auto_failover = status.auto_failover;
                            let peer_is_active_owner =
                                merge_direct_peer_status(status, &peer.node_id, cfg, true);
                            adopt_auto_failover_from_peer_view(
                                peer_auto_failover,
                                peer_is_active_owner,
                                &peer.node_id,
                                cfg,
                            )
                            .await;
                        } else {
                            tracing::debug!(
                            "Cluster heartbeat response from {} failed identity/freshness validation",
                            peer.node_id
                        );
                            mark_peer_unreachable(&peer.node_id, cfg);
                        }
                    } else {
                        tracing::debug!(
                            "Cluster heartbeat response from {} had no status",
                            peer.node_id
                        );
                        mark_peer_unreachable(&peer.node_id, cfg);
                    }
                }
                Ok(envelope) => {
                    tracing::debug!(
                        "Cluster heartbeat rejected by {}: {:?}",
                        peer.node_id,
                        envelope.message
                    );
                    mark_peer_unreachable(&peer.node_id, cfg);
                }
                Err(e) => {
                    tracing::debug!("Cluster heartbeat parse failed for {}: {}", peer.node_id, e);
                    mark_peer_unreachable(&peer.node_id, cfg);
                }
            }
        }
        Err(e) => {
            tracing::debug!("Cluster heartbeat failed for {}: {}", peer.node_id, e);
            mark_peer_unreachable(&peer.node_id, cfg);
        }
    }
}

pub(crate) fn mark_peer_unreachable(node_id: &str, cfg: &Config) {
    let mut state = cluster_state_write();
    let now = now_secs();
    let failures = if let Some(count) = state.heartbeat_failures.get_mut(node_id) {
        *count = count.saturating_add(1);
        *count
    } else {
        state.heartbeat_failures.insert(node_id.to_string(), 1);
        1
    };
    let peer = cfg
        .cluster
        .peers
        .iter()
        .find(|peer| peer.node_id == node_id);
    if !state.nodes.contains_key(node_id) {
        let node = if let Some(peer) = peer {
            empty_node(
                &peer.node_id,
                &peer.name,
                &peer.api_url,
                peer.priority,
                false,
                now,
            )
        } else {
            empty_node(node_id, node_id, "", 0, false, now)
        };
        state.nodes.insert(node_id.to_string(), node);
    }
    let Some(node) = state.nodes.get_mut(node_id) else {
        return;
    };
    let stale = is_stale(node, cfg, now);
    if failures < HEARTBEAT_FAILURE_THRESHOLD && !stale {
        tracing::debug!(
            "Cluster heartbeat failure for {} ({}/{})",
            node_id,
            failures,
            HEARTBEAT_FAILURE_THRESHOLD
        );
        return;
    }
    // Asymmetric partition guard: if the peer's own inbound heartbeats are
    // still fresh, it is demonstrably alive even though we cannot reach its
    // API. Leave its health to normalize_node_health instead of latching
    // api_unreachable here.
    let recently_seen = node
        .last_seen
        .is_some_and(|last_seen| !last_seen_is_stale(last_seen, cfg, now));
    if recently_seen {
        tracing::debug!(
            "Cluster heartbeat outbound failures for {} ({}) but inbound heartbeat is fresh; not marking unreachable",
            node_id,
            failures
        );
        return;
    }
    node.health = ClusterHealth::unhealthy("api_unreachable", true, false);
}

#[cfg(test)]
mod handoff_log_tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture() -> (tracing::Dispatch, CapturedLogs) {
        let logs = CapturedLogs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        (tracing::Dispatch::new(subscriber), logs)
    }

    fn held_view() -> HandoffView {
        HandoffView {
            owner: Some("ca".into()),
            block: Some("集群节点交接尚未确认，暂停本节点执行".into()),
        }
    }

    fn failed_round(report: &AutomaticHandoffLog, view: &HandoffView, now: Instant, status: u16) {
        report.begin_attempt(view, now);
        report.observe(HandoffProgress::Stage(HandoffStage::StopSource), view, now);
        let error = NodeModeError::Unreachable(format!("节点上游暂不可用（HTTP {status}）"));
        for _ in 0..3 {
            report.observe(HandoffProgress::FailedRequest(error.clone()), view, now);
        }
        report.observe(HandoffProgress::FailedStage(error), view, now);
        report.finish(&Err("本轮源停播未获确认".into()), view, now);
    }

    #[test]
    fn alternating_gateway_failures_share_an_episode_and_finish_immediately() {
        let (dispatch, logs) = capture();
        let start = Instant::now();
        let held = held_view();
        tracing::dispatcher::with_default(&dispatch, || {
            let report = AutomaticHandoffLog::new("ca", "jp", "ca", &held, start);
            for round in 0..12 {
                failed_round(
                    &report,
                    &held,
                    start + Duration::from_secs(15 * round),
                    if round % 2 == 0 { 530 } else { 502 },
                );
            }
            let pending = logs.text();
            assert_eq!(pending.matches("开始；选举改选").count(), 1);
            assert_eq!(
                pending
                    .lines()
                    .filter(|line| line.trim_start().starts_with("WARN "))
                    .count(),
                3,
                "{pending}"
            );
            assert!(pending.contains("HTTP=[530,502]"));
            assert!(pending.contains("轮次=9 模式更新失败=27 耗时=2分"));
            assert!(!pending.contains("已完成"));
            assert!(!pending.contains("已取消"));

            // Completion is less than 60s after the last summary; it must emit
            // immediately and include every suppressed round/request failure.
            let now = start + Duration::from_secs(166);
            report.begin_attempt(&held, now);
            report.observe(
                HandoffProgress::SourceStopped(SourceStopEvidence::Confirmed),
                &held,
                now,
            );
            report.observe(
                HandoffProgress::Stage(HandoffStage::ConfirmOwner),
                &held,
                now,
            );
            report.finish(
                &Ok(()),
                &HandoffView {
                    owner: Some("ca".into()),
                    block: None,
                },
                now,
            );
            assert!(!report.matches("jp", "ca"));
        });
        let output = logs.text();
        assert_eq!(output.matches("本轮交接已完成").count(), 1);
        assert!(output.contains("13轮/2分46秒"));
        assert!(output.contains("模式更新失败=36"));
        assert!(output.contains("本机执行=允许"));
        assert!(!output.contains("正在推流"));
    }

    #[test]
    fn quorum_deferral_and_changed_execution_blocks_are_not_hidden() {
        let (dispatch, logs) = capture();
        let start = Instant::now();
        let held = held_view();
        tracing::dispatcher::with_default(&dispatch, || {
            let report = AutomaticHandoffLog::new("ca", "jp", "ca", &held, start);
            for tick in 0..12 {
                report.defer(
                    "缺少新鲜心跳多数派",
                    &held,
                    start + Duration::from_secs(15 * tick),
                );
            }
            assert_eq!(logs.text().matches("暂缓：缺少新鲜心跳多数派").count(), 3);
            assert!(!logs.text().contains("本轮交接已完成"));
            assert!(logs.text().contains("轮次=0 模式更新失败=0"));

            let now = start + Duration::from_secs(166);
            failed_round(&report, &held, now, 530);
            let lost_quorum = HandoffView {
                owner: Some("ca".into()),
                block: Some("集群心跳多数派已丢失".into()),
            };
            // Same error, new actual gate: report now, without waiting 60s.
            report.finish(&Err("HTTP 530".into()), &lost_quorum, now);
        });
        let output = logs.text();
        assert!(output.contains("已选出目标且多数派条件已满足，继续交接"));
        assert!(output
            .lines()
            .last()
            .unwrap()
            .contains("本机执行=受阻（集群心跳多数派已丢失）"));
        assert!(!output.contains("转播成功"));
    }

    #[test]
    fn changed_assignment_stops_retries_but_reports_the_running_round_result() {
        let (dispatch, logs) = capture();
        let now = Instant::now();
        let held = held_view();
        tracing::dispatcher::with_default(&dispatch, || {
            let first = AutomaticHandoffLog::new("ca", "jp", "ca", &held, now);
            first.begin_attempt(&held, now);
            first.stop_retries("已改选节点 ny", &held, now);
            assert!(
                !first.lock().finished,
                "the in-flight batch has not actually ended"
            );
            assert!(logs.text().contains("已有批次仍待实际结果"));
            first.finish(&Err("当前活跃节点为 ny".into()), &held, now);
            assert!(first.lock().finished);

            let next = AutomaticHandoffLog::new("ca", "ny", "ca", &held, now);
            failed_round(&next, &held, now, 502);
            assert_ne!(first.id, next.id);
        });
        let output = logs.text();
        assert_eq!(output.matches("开始；选举改选").count(), 2);
        assert!(output.contains("本轮未完成，后续自动重试已停止"));
        assert!(output
            .lines()
            .any(|line| line.contains("[ca] 自动交接 ny→ca") && line.contains("WARN")));
    }

    #[test]
    fn completion_keeps_unconfirmed_demotion_and_execution_holds_explicit() {
        let (dispatch, logs) = capture();
        let now = Instant::now();
        tracing::dispatcher::with_default(&dispatch, || {
            for block in [None, held_view().block, Some("集群心跳多数派已丢失".into())] {
                let view = HandoffView {
                    owner: Some("ca".into()),
                    block,
                };
                let report = AutomaticHandoffLog::new("ca", "jp", "ca", &view, now);
                report.begin_attempt(&view, now);
                report.observe(
                    HandoffProgress::SourceStopped(SourceStopEvidence::Unconfirmed),
                    &view,
                    now,
                );
                report.observe(
                    HandoffProgress::Stage(HandoffStage::ConfirmOwner),
                    &view,
                    now,
                );
                report.finish(&Ok(()), &view, now);
            }
        });
        let output = logs.text();
        let completed: Vec<_> = output
            .lines()
            .filter(|line| line.contains("本轮交接已完成"))
            .collect();
        assert_eq!(completed.len(), 3);
        assert!(completed
            .iter()
            .all(|line| line.contains("源停播=本次未确认，已超过自隔离时限")));
        assert!(completed[1].contains("本机执行=受阻"));
        assert!(completed[2].contains("缺少心跳多数派"));
        for false_claim in ["正在推流", "转播成功", "已确认源节点", "已放弃"] {
            assert!(!output.contains(false_claim), "{output}");
        }
    }

    #[test]
    fn source_stop_evidence_changes_are_reported_between_retry_rounds() {
        let (dispatch, logs) = capture();
        let now = Instant::now();
        let held = held_view();
        tracing::dispatcher::with_default(&dispatch, || {
            let report = AutomaticHandoffLog::new("ca", "jp", "ca", &held, now);
            report.begin_attempt(&held, now);
            report.observe(HandoffProgress::Stage(HandoffStage::StopSource), &held, now);
            report.observe(
                HandoffProgress::SourceStopped(SourceStopEvidence::Unconfirmed),
                &held,
                now,
            );
            report.observe(
                HandoffProgress::Stage(HandoffStage::EnableTarget),
                &held,
                now,
            );
            report.finish(&Err("目标模式更新被拒绝".into()), &held, now);

            let retry = now + Duration::from_secs(1);
            report.begin_attempt(&held, retry);
            report.observe(
                HandoffProgress::Stage(HandoffStage::StopSource),
                &held,
                retry,
            );
            report.observe(
                HandoffProgress::SourceStopped(SourceStopEvidence::Confirmed),
                &held,
                retry,
            );
            // Reobserving the same evidence adds no duplicate phase record.
            report.observe(
                HandoffProgress::SourceStopped(SourceStopEvidence::Confirmed),
                &held,
                retry,
            );
            report.finish(&Ok(()), &held, retry);
        });
        let output = logs.text();
        let phases: Vec<_> = output
            .lines()
            .filter(|line| line.contains("源停播校验已结束"))
            .collect();
        assert_eq!(phases.len(), 2, "{output}");
        assert!(phases[0].contains("源停播=本次未确认"));
        assert!(phases[1].contains("源停播=已确认"));
        assert_eq!(output.matches("本轮交接已完成").count(), 1);
        assert!(!output.contains("已确认源节点 jp 停止并关闭监控"));
    }

    #[test]
    fn diagnostics_are_safe_in_fallback_failure_and_terminal_records() {
        let (dispatch, logs) = capture();
        let now = Instant::now();
        let held = held_view();
        let diagnostic = "HTTP 530 https://proxy-user:SYNTHETIC-PROXY@proxy.invalid/path\nCookie: SESSDATA=SYNTHETIC-COOKIE\nAuthorization: Bearer SYNTHETIC-TOKEN";
        tracing::dispatcher::with_default(&dispatch, || {
            let report = AutomaticHandoffLog::new("ca", "jp", "ca", &held, now);
            report.begin_attempt(&held, now);
            report.observe(
                HandoffProgress::ConfigFallback(diagnostic.into()),
                &held,
                now,
            );
            report.observe(
                HandoffProgress::FailedRequest(NodeModeError::Unreachable(diagnostic.into())),
                &held,
                now,
            );
            report.finish(&Err(diagnostic.into()), &held, now);

            let other = AutomaticHandoffLog::new("ca", "ny", "ca", &held, now);
            other.begin_attempt(&held, now);
            other.finish(&Err(diagnostic.into()), &held, now);
            other.stop_retries(diagnostic, &held, now);
        });
        let output = logs.text();
        for secret in [
            "SYNTHETIC-PROXY",
            "SYNTHETIC-COOKIE",
            "SYNTHETIC-TOKEN",
            "proxy.invalid",
        ] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.contains("HTTP 530"));
        assert!(output.lines().all(
            |line| ["INFO", "WARN", "DEBUG"].contains(&line.split_whitespace().next().unwrap())
        ));
    }

    #[tokio::test]
    async fn actual_source_failure_reports_its_stage_and_keeps_target_execution_held() {
        use crate::cluster::state::ClusterState;
        use crate::cluster::tests::ClusterStateGuard;
        use crate::test_support::config as test_config;
        use axum::{
            http::StatusCode,
            routing::{get, post},
            Router,
        };

        let _guard = ClusterStateGuard::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let export_calls = Arc::clone(&calls);
        let mode_calls = Arc::clone(&calls);
        let app = Router::new()
            .route(
                "/api/cluster/export-config",
                get(move || {
                    export_calls.lock().unwrap().push("export");
                    async { StatusCode::from_u16(530).unwrap() }
                }),
            )
            .route(
                "/api/cluster/apply-node-mode",
                post(move || {
                    let mut calls = mode_calls.lock().unwrap();
                    calls.push("disable");
                    let status = if calls.len() % 2 == 0 { 530 } else { 502 };
                    async move { StatusCode::from_u16(status).unwrap() }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut cfg = test_config("ca", 100);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "jp".into(),
            name: "JP".into(),
            api_url: format!("http://{address}"),
            priority: 50,
        }];
        let mut nodes: Vec<_> = ["ca", "jp"]
            .into_iter()
            .map(|id| {
                let mut node = empty_node(
                    id,
                    id,
                    "",
                    if id == "ca" { 100 } else { 50 },
                    id == "ca",
                    now_secs(),
                );
                node.last_seen = Some(now_secs());
                node.health = ClusterHealth::healthy();
                node
            })
            .collect();
        nodes[1].ffmpeg_running = true;
        let before = ClusterStatus {
            enabled: true,
            local_node_id: "ca".into(),
            active_owner: Some("ca".into()),
            lease_until: None,
            config_version: String::new(),
            auto_failover: true,
            public_status: Default::default(),
            nodes,
        };
        *cluster_state_write() = ClusterState {
            active_owner: Some("ca".into()),
            pending_handoff_source: Some("jp".into()),
            nodes: before
                .nodes
                .iter()
                .map(|node| (node.node_id.clone(), node.clone()))
                .collect(),
            ..Default::default()
        };
        let (dispatch, logs) = capture();
        let report = tracing::dispatcher::with_default(&dispatch, || {
            let report = Arc::new(AutomaticHandoffLog::new(
                "ca",
                "jp",
                "ca",
                &HandoffView::current(&cfg),
                Instant::now(),
            ));
            report.begin_attempt(&HandoffView::current(&cfg), Instant::now());
            report
        });
        let progress = Arc::new(Mutex::new(Vec::new()));
        let observed_progress = Arc::clone(&progress);
        let observed_report = Arc::clone(&report);
        let observed_cfg = cfg.clone();
        let observed_dispatch = dispatch.clone();
        let observer: HandoffObserver = Arc::new(move |event| {
            observed_progress.lock().unwrap().push(event.clone());
            tracing::dispatcher::with_default(&observed_dispatch, || {
                observed_report.observe(
                    event,
                    &HandoffView::current(&observed_cfg),
                    Instant::now(),
                );
            });
        });
        let result = super::super::sync::finalize_cluster_node_switch_observed(
            &cfg,
            &before,
            "jp",
            "ca",
            false,
            true,
            Some(observer),
        )
        .await;
        server.abort();
        tracing::dispatcher::with_default(&dispatch, || {
            report.finish(&result, &HandoffView::current(&cfg), Instant::now());
        });

        assert!(result.unwrap_err().contains("源节点尚未确认停止"));
        assert_eq!(
            *calls.lock().unwrap(),
            ["export", "disable", "disable", "disable"]
        );
        let events = progress.lock().unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, HandoffProgress::Stage(HandoffStage::StopSource))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, HandoffProgress::Stage(HandoffStage::EnableTarget))));
        let state = cluster_state_read();
        assert_eq!(state.pending_handoff_source.as_deref(), Some("jp"));
        assert!(state.unconfirmed_demotion.is_none());
        drop(state);
        assert!(local_monitoring_block_reason(&cfg).is_some());
        let output = logs.text();
        assert!(output.contains("阶段=确认源停播"));
        assert!(output.contains("所选=ca；本机执行=受阻"));
        assert!(output.contains("类别=unreachable"));
        assert!(!output.contains("本轮交接已完成"));
    }

    #[tokio::test]
    async fn owed_demotion_recovery_logs_once_after_a_real_acknowledgement() {
        use crate::cluster::state::ClusterState;
        use crate::cluster::tests::ClusterStateGuard;
        use crate::test_support::config as test_config;
        use axum::{routing::post, Json, Router};
        use tracing::instrument::WithSubscriber;

        let _guard = ClusterStateGuard::new();
        let mut source = empty_node("jp", "JP", "", 50, false, now_secs());
        source.last_seen = Some(now_secs());
        source.monitor_toggles = all_monitor_toggles_off();
        let reply = ClusterStatus {
            enabled: true,
            local_node_id: "jp".into(),
            active_owner: Some("ca".into()),
            lease_until: None,
            config_version: String::new(),
            auto_failover: true,
            public_status: Default::default(),
            nodes: vec![source],
        };
        let calls = Arc::new(AtomicU64::new(0));
        let request_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/api/cluster/apply-node-mode",
            post(move || {
                request_calls.fetch_add(1, Ordering::Relaxed);
                let reply = reply.clone();
                async move { Json(serde_json::json!({"success": true, "data": reply})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut cfg = test_config("ca", 100);
        cfg.cluster.peers = vec![crate::config::ClusterPeer {
            node_id: "jp".into(),
            name: "JP".into(),
            api_url: format!("http://{address}"),
            priority: 50,
        }];
        *cluster_state_write() = ClusterState {
            active_owner: Some("ca".into()),
            unconfirmed_demotion: Some("jp".into()),
            ..Default::default()
        };
        let (dispatch, logs) = capture();
        for _ in 0..2 {
            retry_unconfirmed_demotion(&cfg)
                .with_subscriber(dispatch.clone())
                .await;
        }
        server.abort();
        assert!(cluster_state_read().unconfirmed_demotion.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let output = logs.text();
        assert_eq!(
            output
                .matches("[ca] 已确认源节点 jp 停止并关闭监控")
                .count(),
            1,
            "{output}"
        );
        assert!(!output.contains("本轮交接已完成"));
    }
}
