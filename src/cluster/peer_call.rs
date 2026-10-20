//! Signed calls between current members, plus the one route table both the
//! sender and the receiving router use. Every call resolves its recipient from
//! one committed membership snapshot; there is no shared credential.

use super::membership::{member_endpoint, trust_snapshot, Descriptor, Membership};
use super::peer_auth::{
    PeerClient, PeerResponse, PeerScope, RoutePolicy, HELLO_REQUEST_BYTES, HELLO_RESPONSE_BYTES,
    MAX_BODY_BYTES, NOTIFY_CREDENTIALS_PATH, NOTIFY_OPENID_PATH, OPERATION_BODY_BYTES,
    PAIRING_BODY_BYTES,
};
// Only `unsigned_test_send` marks the watch-sync pull, so the import exists in
// test builds alone.
#[cfg(test)]
use super::peer_auth::NOTIFY_WATCH_SYNC_HEADER;
use super::types::PeerApiResponse;
use crate::config::Config;
use serde::{de::DeserializeOwned, Serialize};
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const fn route(
    method: &'static str,
    path: &'static str,
    max_request_bytes: usize,
    max_response_bytes: usize,
) -> RoutePolicy {
    RoutePolicy {
        method,
        path,
        max_request_bytes,
        max_response_bytes,
    }
}

pub(crate) mod routes {
    use super::*;
    pub(crate) const HEARTBEAT: RoutePolicy = route(
        "POST",
        "/api/cluster/heartbeat",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const SELF_CHECK: RoutePolicy = route("POST", "/api/cluster/self-check", 1024, 4096);
    pub(crate) const CAPABILITIES: RoutePolicy = route("GET", "/api/cluster/capabilities", 0, 4096);
    pub(crate) const EXPORT_CONFIG: RoutePolicy =
        route("GET", "/api/cluster/export-config", 0, MAX_BODY_BYTES);
    /// The owner caps its index at 2,000 answers of roughly 250 bytes each.
    pub(crate) const YT_INDEX: RoutePolicy =
        route("GET", "/api/cluster/yt-index", 0, MAX_BODY_BYTES);
    /// 统计 read for `scope=cluster` (analytics §8): validated query in,
    /// the peer's own mergeable parts out. Read-only.
    pub(crate) const HISTORY: RoutePolicy =
        route("POST", "/api/cluster/v1/history", 16 * 1024, 1024 * 1024);
    /// 清理历史 across the cluster (analytics §11): a member asks this node
    /// to delete its own recorded rows. Members only, and a node that receives
    /// it never fans out again, so one click deletes each archive once.
    pub(crate) const HISTORY_CLEAN: RoutePolicy =
        route("POST", "/api/cluster/v1/history/clean", 1024, 4096);
    pub(crate) const APPLY_NODE_MODE: RoutePolicy = route(
        "POST",
        "/api/cluster/apply-node-mode",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const SYNC_CONFIG: RoutePolicy = route(
        "POST",
        "/api/cluster/sync-config",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const CACHE_MONITOR_STATE: RoutePolicy = route(
        "POST",
        "/api/cluster/cache-active-monitor-state",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const APPLY_PUBLIC_STATUS: RoutePolicy = route(
        "POST",
        "/api/cluster/apply-public-status",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const SETTINGS: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/settings",
        OPERATION_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const FORWARD: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/membership/forward",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    pub(crate) const DRAIN: RoutePolicy =
        route("POST", "/api/cluster/drain", MAX_BODY_BYTES, MAX_BODY_BYTES);
    pub(crate) const AUTO_FAILOVER: RoutePolicy = route(
        "POST",
        "/api/cluster/auto-failover",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const FAILOVER: RoutePolicy = route(
        "POST",
        "/api/cluster/failover",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const RESTART: RoutePolicy = route(
        "POST",
        "/api/server/restart",
        MAX_BODY_BYTES,
        MAX_BODY_BYTES,
    );
    pub(crate) const PREPARE: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/operation/prepare",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    pub(crate) const DECISION: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/operation/decision",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    pub(crate) const ABORT: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/operation/abort",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    pub(crate) const FINISH: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/operation/finish",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    pub(crate) const STATUS: RoutePolicy = route(
        "POST",
        "/api/cluster/v1/operation/status",
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    /// Batch 1b: the writer pushes the sealed admin openid record (or an
    /// explicit unbound state) to a current member.
    pub(crate) const NOTIFY_OPENID_PUSH: RoutePolicy =
        route("POST", NOTIFY_OPENID_PATH, 64 * 1024, 64 * 1024);
    /// Batch 1b: a joining node or a new writer pulls a member's record once.
    pub(crate) const NOTIFY_OPENID_PULL: RoutePolicy =
        route("GET", NOTIFY_OPENID_PATH, 0, 64 * 1024);
    /// Addendum F: a member forwards an AppID/AppSecret save to the gateway
    /// holder. The body may also carry a follow list of up to 50 entries.
    /// `POST` on its own path: a `PUT` route can never be signed
    /// (`RoutePolicy::validate`), which refused every forward before sending.
    pub(crate) const NOTIFY_CREDENTIALS_SAVE: RoutePolicy =
        route("POST", NOTIFY_CREDENTIALS_PATH, 64 * 1024, 64 * 1024);
    /// Unauthenticated bootstrap: identity proof and password-gated pairing.
    pub(crate) const HELLO: RoutePolicy = route(
        "POST",
        super::super::peer_auth::HELLO_ROUTE,
        HELLO_REQUEST_BYTES,
        HELLO_RESPONSE_BYTES,
    );
    pub(crate) const PAIRING: RoutePolicy = route(
        "POST",
        super::super::peer_auth::PAIRING_ROUTE,
        PAIRING_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
    /// Not current-membership authority. The caller may present a stale revision.
    pub(crate) const RECOGNITION: RoutePolicy = route(
        "POST",
        super::super::peer_auth::RECOGNITION_ROUTE,
        OPERATION_BODY_BYTES,
        OPERATION_BODY_BYTES,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteClass {
    /// Signed current-membership authority only.
    PeerOnly,
    /// Signed member or ordinary browser session.
    Control,
    /// Signed exact-operation authority; trust comes from the body's intent.
    Operation,
    Bootstrap,
    /// Signed probe of one key. Grants no peer route and writes nothing.
    Recognition,
}

const TABLE: &[(RoutePolicy, RouteClass)] = &[
    (routes::HEARTBEAT, RouteClass::PeerOnly),
    (routes::SELF_CHECK, RouteClass::PeerOnly),
    (routes::CAPABILITIES, RouteClass::PeerOnly),
    (routes::EXPORT_CONFIG, RouteClass::PeerOnly),
    (routes::YT_INDEX, RouteClass::PeerOnly),
    (routes::HISTORY, RouteClass::PeerOnly),
    (routes::HISTORY_CLEAN, RouteClass::PeerOnly),
    (routes::APPLY_NODE_MODE, RouteClass::PeerOnly),
    (routes::SYNC_CONFIG, RouteClass::PeerOnly),
    (routes::CACHE_MONITOR_STATE, RouteClass::PeerOnly),
    (routes::APPLY_PUBLIC_STATUS, RouteClass::PeerOnly),
    (routes::SETTINGS, RouteClass::PeerOnly),
    (routes::FORWARD, RouteClass::PeerOnly),
    (routes::NOTIFY_OPENID_PUSH, RouteClass::PeerOnly),
    (routes::NOTIFY_OPENID_PULL, RouteClass::PeerOnly),
    (routes::NOTIFY_CREDENTIALS_SAVE, RouteClass::PeerOnly),
    (routes::DRAIN, RouteClass::Control),
    (routes::AUTO_FAILOVER, RouteClass::Control),
    (routes::FAILOVER, RouteClass::Control),
    (routes::RESTART, RouteClass::Control),
    (routes::PREPARE, RouteClass::Operation),
    (routes::DECISION, RouteClass::Operation),
    (routes::ABORT, RouteClass::Operation),
    (routes::FINISH, RouteClass::Operation),
    (routes::STATUS, RouteClass::Operation),
    (routes::HELLO, RouteClass::Bootstrap),
    (routes::PAIRING, RouteClass::Bootstrap),
    (routes::RECOGNITION, RouteClass::Recognition),
];

/// Exact method + logical path. Aliases, trailing slashes and other methods
/// never match.
pub(crate) fn peer_route(method: &str, path: &str) -> Option<(RoutePolicy, RouteClass)> {
    TABLE
        .iter()
        .find(|(route, _)| route.method == method && route.path == path)
        .copied()
}

/// Any method on a peer-only or operation path is unavailable to browsers.
pub(crate) fn browser_forbidden(path: &str) -> bool {
    path.starts_with("/api/cluster/v1/")
        || TABLE.iter().any(|(route, class)| {
            route.path == path
                && matches!(
                    class,
                    RouteClass::PeerOnly | RouteClass::Operation | RouteClass::Recognition
                )
        })
}

pub(crate) fn peer_client() -> io::Result<Arc<PeerClient>> {
    static CLIENT: OnceLock<Arc<PeerClient>> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(Arc::clone(client));
    }
    let client = Arc::new(PeerClient::new(Duration::from_secs(30))?);
    Ok(Arc::clone(CLIENT.get_or_init(|| client)))
}

/// Why a peer call failed: the node did not answer (down, timeout, or its
/// tunnel has no origin), or it answered and refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerCallError {
    Unreachable(String),
    Refused(String),
}

impl std::fmt::Display for PeerCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(message) | Self::Refused(message) => f.write_str(message),
        }
    }
}

pub(crate) fn classify(error: io::Error) -> PeerCallError {
    match error.kind() {
        io::ErrorKind::TimedOut
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::Other => PeerCallError::Unreachable(error.to_string()),
        _ => PeerCallError::Refused(error.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_to_member(
    membership: &Membership,
    client: &PeerClient,
    member: &Descriptor,
    cluster_id: &str,
    revision: u64,
    scope: PeerScope,
    route: RoutePolicy,
    body: Vec<u8>,
    timeout: Duration,
) -> io::Result<PeerResponse> {
    let identity = membership.identity()?;
    let endpoint = member_endpoint(&member.api_url)?;
    tokio::time::timeout(
        timeout,
        client.send(
            &identity,
            &endpoint,
            &member.identity(),
            cluster_id,
            revision,
            scope,
            route,
            body,
        ),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "节点请求超时"))?
}

/// Ordinary current-membership call addressed by friendly node ID. The
/// address comes from the committed manifest, never from editable config.
pub(crate) async fn send_ordinary_with(
    membership: &Membership,
    client: &PeerClient,
    node_id: &str,
    route: RoutePolicy,
    body: Vec<u8>,
    timeout: Duration,
) -> io::Result<PeerResponse> {
    let trust = trust_snapshot(membership.store())?;
    let manifest = membership
        .manifest()?
        .filter(|m| trust.revisions == [m.revision] && m.cluster_id == trust.cluster_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "成员版本正在变化"))?;
    let member = manifest
        .members
        .iter()
        .find(|m| m.node_id == node_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "节点不在当前成员清单"))?;
    send_to_member(
        membership,
        client,
        member,
        &manifest.cluster_id,
        manifest.revision,
        PeerScope::Ordinary,
        route,
        body,
        timeout,
    )
    .await
}

pub(crate) async fn send_ordinary(
    cfg: &Config,
    node_id: &str,
    route: RoutePolicy,
    body: Vec<u8>,
    timeout: Duration,
) -> io::Result<PeerResponse> {
    let membership = Membership::new(crate::storage::global()?);
    #[cfg(test)]
    if trust_snapshot(membership.store()).is_err() {
        return unsigned_test_send(cfg, node_id, route, body, timeout).await;
    }
    let _ = cfg;
    send_ordinary_with(&membership, &*peer_client()?, node_id, route, body, timeout).await
}

/// Signed call expecting a 2xx `PeerApiResponse` envelope.
pub(crate) async fn call<T: Serialize, R: DeserializeOwned>(
    cfg: &Config,
    node_id: &str,
    route: RoutePolicy,
    body: Option<&T>,
    timeout: Duration,
) -> Result<PeerApiResponse<R>, PeerCallError> {
    let body = match body {
        Some(body) => serde_json::to_vec(body)
            .map_err(|_| PeerCallError::Refused("节点请求编码失败".into()))?,
        None => Vec::new(),
    };
    let response = send_ordinary(cfg, node_id, route, body, timeout)
        .await
        .map_err(classify)?;
    if !(200..300).contains(&response.status) {
        return Err(PeerCallError::Refused(format!("HTTP {}", response.status)));
    }
    serde_json::from_slice(&response.body)
        .map_err(|_| PeerCallError::Refused("节点响应格式无效".into()))
}

/// Inserted by the admin router after signed admission. `node_id` is empty
/// for an operation participant that is not yet a committed member.
#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedNode {
    pub(crate) member_id: String,
    pub(crate) node_id: String,
}

/// Friendly node ID of a currently committed member.
pub(crate) fn member_node_id(store: &crate::storage::Store, member_id: &str) -> Option<String> {
    let manifest: super::membership::Manifest = store
        .read(super::membership::MANIFEST_RECORD)
        .ok()
        .flatten()
        .and_then(|doc| serde_json::from_value(doc.value).ok())?;
    manifest.member(member_id).map(|m| m.node_id.clone())
}

#[cfg(test)]
async fn unsigned_test_send(
    cfg: &Config,
    node_id: &str,
    route: RoutePolicy,
    body: Vec<u8>,
    timeout: Duration,
) -> io::Result<PeerResponse> {
    // Cluster logic tests run without managed membership; their mock peers
    // speak plain JSON. Signed transport is covered by the real-router tests.
    // Same route rule as signing: a route the signed path could never send
    // must fail here too, or tests pass a call production refuses.
    route.validate()?;
    let base = if node_id == cfg.cluster.node_id {
        cfg.cluster.public_api_url.clone()
    } else {
        cfg.cluster
            .peers
            .iter()
            .find(|peer| peer.node_id == node_id)
            .map(|peer| peer.api_url.clone())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "节点不在当前成员清单"))?
    };
    crate::install_crypto_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(io::Error::other)?;
    let url = format!("{}{}", base.trim_end_matches('/'), route.path);
    let mut request = if route.method == "GET" {
        client.get(url)
    } else {
        client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
    };
    if route.method == "GET" && route.path == routes::NOTIFY_OPENID_PULL.path {
        request = request.header(NOTIFY_WATCH_SYNC_HEADER, "1");
    }
    let response = request.timeout(timeout).send().await.map_err(|e| {
        io::Error::new(
            if e.is_timeout() {
                io::ErrorKind::TimedOut
            } else {
                io::ErrorKind::ConnectionRefused
            },
            "节点连接失败",
        )
    })?;
    let status = response.status().as_u16();
    if matches!(status, 502..=504 | 520..=530) {
        return Err(super::peer_auth::unsigned_error(status));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = crate::plugins::http::response_bytes_limited(response, route.max_response_bytes)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "节点响应无效或过大"))?;
    Ok(PeerResponse {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod route_table_tests {
    use super::*;

    /// Regression (ny/ca watch-list saves): a route in this table that
    /// `RoutePolicy::validate` refuses can never be signed, so every call
    /// on it fails before leaving the sender. The forwarded save was a `PUT`.
    #[test]
    fn every_peer_route_in_the_table_can_be_signed() {
        for (route, class) in TABLE {
            assert!(
                route.validate().is_ok(),
                "{} {} ({class:?}) cannot be signed",
                route.method,
                route.path
            );
        }
        assert_eq!(routes::NOTIFY_CREDENTIALS_SAVE.method, "POST");
        assert_eq!(
            routes::NOTIFY_CREDENTIALS_SAVE.path,
            "/api/cluster/v1/notify/credentials"
        );
        assert_eq!(routes::NOTIFY_CREDENTIALS_SAVE.max_request_bytes, 64 * 1024);
        assert_eq!(
            routes::NOTIFY_CREDENTIALS_SAVE.max_response_bytes,
            64 * 1024
        );
        assert!(matches!(
            peer_route("POST", routes::NOTIFY_CREDENTIALS_SAVE.path),
            Some((_, RouteClass::PeerOnly))
        ));
    }

    /// L2: the unsigned test transport applies the same route rule as
    /// signing, before any address lookup or network call.
    #[tokio::test]
    async fn the_unsigned_test_transport_refuses_a_route_that_cannot_be_signed() {
        let cfg = crate::test_support::config("l2", 0);
        let put = RoutePolicy {
            method: "PUT",
            path: NOTIFY_CREDENTIALS_PATH,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
        };
        let error = unsigned_test_send(&cfg, "nowhere", put, Vec::new(), Duration::from_secs(1))
            .await
            .err()
            .expect("a PUT route is refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // A valid route gets past the rule and fails only on the unknown node.
        let error = unsigned_test_send(
            &cfg,
            "nowhere",
            routes::NOTIFY_CREDENTIALS_SAVE,
            Vec::new(),
            Duration::from_secs(1),
        )
        .await
        .err()
        .expect("unknown node");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
