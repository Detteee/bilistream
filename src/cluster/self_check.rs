//! Reach the local control endpoint through its advertised URL (including a
//! Cloudflare tunnel). This observation never contributes a quorum vote.

use super::state::{cluster_heartbeat_timeout, cluster_state_write, now_secs};
use super::types::PeerApiResponse;
use crate::config::{ClusterConfig, Config};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SELF_CHECK_API_PATH: &str = "/cluster/self-check";
const MAX_RESPONSE_BYTES: usize = 4096;
const FAILURE_THRESHOLD: u32 = 3;
static NEXT_CHALLENGE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SelfCheckState {
    #[default]
    Pending,
    Healthy,
    Failing,
    Unreachable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SelfCheckFailure {
    InvalidUrl,
    RequestFailed,
    TimedOut,
    HttpStatus(u16),
    BodyTooLarge,
    InvalidResponse,
    IdentityMismatch,
    ChallengeMismatch,
    Rejected,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelfCheckStatus {
    pub state: SelfCheckState,
    pub consecutive_failures: u32,
    pub checked_at: Option<u64>,
    pub last_success_at: Option<u64>,
    pub latency_ms: Option<u64>,
    pub failure: Option<SelfCheckFailure>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SelfCheckRequest {
    pub challenge: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SelfCheckReply {
    pub node_id: String,
    pub challenge: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SelfCheckRecord {
    node_id: String,
    public_api_url: String,
    observed_at: Instant,
    status: SelfCheckStatus,
}

fn configured_url(cluster: &ClusterConfig) -> &str {
    cluster.public_api_url.trim().trim_end_matches('/')
}

fn freshness(cluster: &ClusterConfig) -> Duration {
    Duration::from_secs(
        cluster
            .failover_timeout_secs
            .max(cluster.heartbeat_interval_secs.saturating_mul(2))
            .max(1),
    )
}

pub(crate) fn self_check_status(
    record: Option<&SelfCheckRecord>,
    cluster: &ClusterConfig,
    now: Instant,
) -> Option<SelfCheckStatus> {
    if !cluster.enabled || configured_url(cluster).is_empty() {
        return None;
    }
    Some(match record {
        Some(record)
            if record.node_id == cluster.node_id
                && record.public_api_url == configured_url(cluster)
                && now.saturating_duration_since(record.observed_at) <= freshness(cluster) =>
        {
            record.status.clone()
        }
        _ => SelfCheckStatus::default(),
    })
}

pub(crate) fn record_self_check(
    slot: &mut Option<SelfCheckRecord>,
    cluster: &ClusterConfig,
    result: Result<u64, SelfCheckFailure>,
    now: Instant,
    wall_time: u64,
) {
    let mut status = self_check_status(slot.as_ref(), cluster, now).unwrap_or_default();
    status.checked_at = Some(wall_time);
    match result {
        Ok(latency) => {
            status.state = SelfCheckState::Healthy;
            status.consecutive_failures = 0;
            status.last_success_at = Some(wall_time);
            status.latency_ms = Some(latency);
            status.failure = None;
        }
        Err(failure) => {
            status.consecutive_failures = status.consecutive_failures.saturating_add(1);
            status.state = if status.consecutive_failures >= FAILURE_THRESHOLD {
                SelfCheckState::Unreachable
            } else {
                SelfCheckState::Failing
            };
            status.latency_ms = None;
            status.failure = Some(failure);
        }
    }
    *slot = Some(SelfCheckRecord {
        node_id: cluster.node_id.clone(),
        public_api_url: configured_url(cluster).to_string(),
        observed_at: now,
        status,
    });
}

fn endpoint_url(base: &str) -> Result<reqwest::Url, SelfCheckFailure> {
    let mut url = reqwest::Url::parse(base).map_err(|_| SelfCheckFailure::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SelfCheckFailure::InvalidUrl);
    }
    url.path_segments_mut()
        .map_err(|_| SelfCheckFailure::InvalidUrl)?
        .pop_if_empty()
        .push("api")
        .extend(SELF_CHECK_API_PATH.trim_start_matches('/').split('/'));
    Ok(url)
}

pub(crate) fn self_check_reply(
    cluster: &ClusterConfig,
    request: SelfCheckRequest,
) -> Option<SelfCheckReply> {
    // The challenge detects cached responses; the API's existing session
    // middleware authenticates the caller. Never accept an unbounded echo.
    (cluster.enabled && !request.challenge.is_empty() && request.challenge.len() <= 128).then(
        || SelfCheckReply {
            node_id: cluster.node_id.clone(),
            challenge: request.challenge,
        },
    )
}

async fn probe(
    cfg: &Config,
    timeout: Duration,
    challenge: String,
) -> Result<u64, SelfCheckFailure> {
    let cluster = &cfg.cluster;
    endpoint_url(configured_url(cluster))?;
    let body = serde_json::to_vec(&SelfCheckRequest {
        challenge: challenge.clone(),
    })
    .map_err(|_| SelfCheckFailure::InvalidResponse)?;
    let started = Instant::now();
    let result = tokio::time::timeout(timeout, async {
        let response = super::peer_call::send_ordinary(
            cfg,
            &cluster.node_id,
            super::peer_call::routes::SELF_CHECK,
            body,
            timeout,
        )
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::TimedOut => SelfCheckFailure::TimedOut,
            std::io::ErrorKind::InvalidData => SelfCheckFailure::InvalidResponse,
            std::io::ErrorKind::PermissionDenied => SelfCheckFailure::Rejected,
            _ => SelfCheckFailure::RequestFailed,
        })?;
        if !(200..300).contains(&response.status) {
            return Err(SelfCheckFailure::HttpStatus(response.status));
        }
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(SelfCheckFailure::BodyTooLarge);
        }
        let envelope: PeerApiResponse<SelfCheckReply> = serde_json::from_slice(&response.body)
            .map_err(|_| SelfCheckFailure::InvalidResponse)?;
        let reply = envelope
            .data
            .filter(|_| envelope.success)
            .ok_or(SelfCheckFailure::Rejected)?;
        if reply.node_id != cluster.node_id {
            return Err(SelfCheckFailure::IdentityMismatch);
        }
        if reply.challenge != challenge {
            return Err(SelfCheckFailure::ChallengeMismatch);
        }
        Ok(started.elapsed().as_millis().min(u64::MAX as u128) as u64)
    })
    .await;
    result.unwrap_or(Err(SelfCheckFailure::TimedOut))
}

pub(crate) async fn run_self_checks() {
    loop {
        let started = Instant::now();
        let cfg = match crate::config::load_config()
            .await
            .map_err(|error| error.to_string())
        {
            Ok(cfg) => cfg,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(15)).await;
                continue;
            }
        };
        if cfg.cluster.enabled && !configured_url(&cfg.cluster).is_empty() {
            let challenge = format!(
                "{:x}-{:x}-{:x}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                NEXT_CHALLENGE.fetch_add(1, Ordering::Relaxed)
            );
            let result = probe(&cfg, cluster_heartbeat_timeout(&cfg), challenge).await;
            // A response to the previous URL/configuration cannot overwrite a
            // newly configured endpoint's status.
            crate::config::with_current_config(&cfg, || {
                record_self_check(
                    &mut cluster_state_write().local_self_check,
                    &cfg.cluster,
                    result,
                    Instant::now(),
                    now_secs(),
                );
            });
        }
        let period = if cfg.cluster.enabled {
            super::heartbeat::heartbeat_sleep_duration(&cfg)
        } else {
            Duration::from_secs(15)
        };
        tokio::time::sleep(period.saturating_sub(started.elapsed())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterPeer;
    use axum::{http::StatusCode, response::IntoResponse, Json};

    fn cluster() -> ClusterConfig {
        ClusterConfig {
            enabled: true,
            node_id: "local".into(),
            public_api_url: "https://local.example/prefix/".into(),
            heartbeat_interval_secs: 5,
            failover_timeout_secs: 30,
            ..Default::default()
        }
    }

    #[test]
    fn endpoint_preserves_tunnel_prefix_and_rejects_ambiguous_urls() {
        assert_eq!(
            endpoint_url("https://node.example/prefix/")
                .unwrap()
                .as_str(),
            "https://node.example/prefix/api/cluster/self-check"
        );
        for value in [
            "file:///tmp/test",
            "https://user:password@node.example",
            "https://node.example/?next=other",
            "https://node.example/#fragment",
            "invalid",
        ] {
            assert_eq!(endpoint_url(value), Err(SelfCheckFailure::InvalidUrl));
        }
        assert!(!crate::webui::listen::is_public_api_path(
            "/api/cluster/self-check"
        ));
    }

    #[test]
    fn reply_echo_is_bounded_and_identifies_the_serving_node() {
        let mut cfg = cluster();
        let reply = self_check_reply(
            &cfg,
            SelfCheckRequest {
                challenge: "fresh".into(),
            },
        )
        .unwrap();
        assert_eq!(reply.node_id, "local");
        assert_eq!(reply.challenge, "fresh");
        for challenge in [String::new(), "x".repeat(129)] {
            assert!(self_check_reply(&cfg, SelfCheckRequest { challenge }).is_none());
        }
        cfg.enabled = false;
        assert!(self_check_reply(
            &cfg,
            SelfCheckRequest {
                challenge: "fresh".into()
            }
        )
        .is_none());
    }

    #[test]
    fn failures_need_a_streak_and_success_recovers_immediately() {
        let cfg = cluster();
        let now = Instant::now();
        let mut record = None;
        for failure in 1..=3 {
            record_self_check(&mut record, &cfg, Err(SelfCheckFailure::TimedOut), now, 100);
            let status = self_check_status(record.as_ref(), &cfg, now).unwrap();
            assert_eq!(status.consecutive_failures, failure);
            assert_eq!(
                status.state,
                if failure < 3 {
                    SelfCheckState::Failing
                } else {
                    SelfCheckState::Unreachable
                }
            );
        }
        record_self_check(&mut record, &cfg, Ok(23), now, 101);
        let status = self_check_status(record.as_ref(), &cfg, now).unwrap();
        assert_eq!(status.state, SelfCheckState::Healthy);
        assert_eq!(status.latency_ms, Some(23));
        assert_eq!(status.last_success_at, Some(101));
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.failure, None);
    }

    #[test]
    fn stale_results_and_changed_targets_cannot_report_current_health() {
        let mut cfg = cluster();
        let now = Instant::now();
        let mut record = None;
        record_self_check(&mut record, &cfg, Ok(10), now, 100);
        let expired = now + freshness(&cfg) + Duration::from_secs(1);
        assert_eq!(
            self_check_status(record.as_ref(), &cfg, expired)
                .unwrap()
                .state,
            SelfCheckState::Pending
        );
        cfg.node_id = "replacement".into();
        assert_eq!(
            self_check_status(record.as_ref(), &cfg, now).unwrap().state,
            SelfCheckState::Pending
        );
        cfg.node_id = "local".into();
        cfg.public_api_url = "https://replacement.example".into();
        record_self_check(
            &mut record,
            &cfg,
            Err(SelfCheckFailure::RequestFailed),
            now,
            101,
        );
        let status = self_check_status(record.as_ref(), &cfg, now).unwrap();
        assert_eq!(status.consecutive_failures, 1);
        assert_eq!(status.last_success_at, None);
        cfg.public_api_url.clear();
        assert!(self_check_status(record.as_ref(), &cfg, now).is_none());
    }

    #[test]
    fn tunnel_failure_needs_peer_failure_evidence_and_never_adds_votes() {
        let mut cfg = cluster();
        cfg.peers = ["a", "b", "c"]
            .map(|node_id| ClusterPeer {
                node_id: node_id.into(),
                name: node_id.into(),
                api_url: format!("https://{node_id}.example"),
                priority: 0,
            })
            .to_vec();
        let mut state = super::super::state::ClusterState::default();
        for _ in 0..3 {
            record_self_check(
                &mut state.local_self_check,
                &cfg,
                Err(SelfCheckFailure::TimedOut),
                Instant::now(),
                100,
            );
        }
        let isolated = |state: &super::super::state::ClusterState| {
            super::super::fencing::local_network_isolated_from_state(&cfg, state, 0, 100)
        };
        assert!(!isolated(&state));
        state.heartbeat_failures.insert("a".into(), 3);
        assert!(!isolated(&state));
        state.heartbeat_failures.insert("b".into(), 3);
        assert!(isolated(&state));
        let mut inbound = super::super::status::empty_node("a", "a", "", 0, false, 100);
        inbound.last_seen = Some(100);
        state.nodes.insert("a".into(), inbound);
        assert!(!isolated(&state));
        assert!(state.peer_heartbeat_acks.is_empty());
        assert!(state.peer_owner_views.is_empty());
        assert!(!state.nodes.contains_key("local"));
    }

    #[test]
    fn ui_signature_tracks_reachability_but_not_probe_timestamps() {
        let mut status = super::super::types::ClusterStatus {
            enabled: true,
            local_node_id: "local".into(),
            active_owner: Some("local".into()),
            lease_until: None,
            config_version: "version".into(),
            auto_failover: true,
            public_status: Default::default(),
            nodes: vec![super::super::status::empty_node(
                "local", "Local", "", 0, true, 100,
            )],
        };
        let node = status
            .nodes
            .iter_mut()
            .find(|node| node.node_id == "local")
            .unwrap();
        node.self_check = Some(SelfCheckStatus {
            state: SelfCheckState::Healthy,
            checked_at: Some(100),
            ..Default::default()
        });
        let before = super::super::status::cluster_ui_signature(&status);
        status
            .nodes
            .iter_mut()
            .find(|node| node.node_id == "local")
            .unwrap()
            .self_check
            .as_mut()
            .unwrap()
            .checked_at = Some(101);
        assert_eq!(before, super::super::status::cluster_ui_signature(&status));
        status
            .nodes
            .iter_mut()
            .find(|node| node.node_id == "local")
            .unwrap()
            .self_check
            .as_mut()
            .unwrap()
            .state = SelfCheckState::Unreachable;
        assert_ne!(before, super::super::status::cluster_ui_signature(&status));
    }

    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn server() -> (Config, Server) {
        crate::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/prefix/api/cluster/self-check", axum::routing::post(
            |Json(request): Json<SelfCheckRequest>| async move {
                match request.challenge.as_str() {
                    "slow" => tokio::time::sleep(Duration::from_secs(1)).await,
                    "oversized" => return axum::body::Body::from_stream(futures_util::stream::iter(
                        (0..10).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024])),
                    )).into_response(),
                    "proxy-error" => return StatusCode::BAD_GATEWAY.into_response(),
                    "login-page" => return "<html>Cloudflare Access</html>".into_response(),
                    _ => {}
                }
                Json(serde_json::json!({"success": true, "data": {
                    "node_id": if request.challenge == "wrong-node" { "other" } else { "local" },
                    "challenge": if request.challenge == "cached" { "old".to_string() } else { request.challenge },
                }})).into_response()
            }
        ));
        let server = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let mut cfg = super::super::tests::test_config("local", 0);
        cfg.cluster = cluster();
        cfg.cluster.public_api_url = format!("http://{address}/prefix/");
        (cfg, server)
    }

    #[tokio::test]
    async fn probe_requires_matching_identity_and_current_challenge() {
        let (cfg, _server) = server().await;
        let deadline = Duration::from_secs(2);
        assert!(probe(&cfg, deadline, "fresh".into()).await.is_ok());
        assert_eq!(
            probe(&cfg, deadline, "wrong-node".into()).await,
            Err(SelfCheckFailure::IdentityMismatch)
        );
        assert_eq!(
            probe(&cfg, deadline, "cached".into()).await,
            Err(SelfCheckFailure::ChallengeMismatch)
        );
    }

    #[tokio::test]
    async fn probe_bounds_latency_and_chunked_bodies_and_rejects_proxy_pages() {
        let (cfg, _server) = server().await;
        let deadline = Duration::from_secs(2);
        assert_eq!(
            probe(&cfg, deadline, "oversized".into()).await,
            Err(SelfCheckFailure::InvalidResponse)
        );
        assert_eq!(
            probe(&cfg, deadline, "proxy-error".into()).await,
            Err(SelfCheckFailure::RequestFailed)
        );
        assert_eq!(
            probe(&cfg, deadline, "login-page".into()).await,
            Err(SelfCheckFailure::InvalidResponse)
        );
        assert_eq!(
            probe(&cfg, Duration::from_millis(20), "slow".into()).await,
            Err(SelfCheckFailure::TimedOut)
        );
    }
}
