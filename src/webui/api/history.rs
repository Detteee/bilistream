//! `/api/history/*` (analytics design §9). Admin router only, inside
//! `require_webui_auth`; never on the public listener. `{success, data}`.

use crate::cluster::peer_call::{routes, PeerCallError};
use crate::config::Config;
use crate::history::query::{
    self, BadRequest, EventsPage, Query, SessionsPage, StatsParts, SummaryParts,
};
use crate::history::MissingNode;
use axum::{
    extract::Query as Params,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

const PEER_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
static QUERIES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn ok(data: Value) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

fn fail(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "success": false, "message": message })),
    )
        .into_response()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Local,
    Node(String),
    Cluster,
}

pub(crate) fn local_node(cfg: &Config) -> String {
    if cfg.cluster.enabled {
        cfg.cluster.node_id.clone()
    } else {
        "local".into()
    }
}

/// `cluster` (default in a cluster), `local` (default on one node), or a
/// known member id. Unknown ids → 400.
pub(crate) fn resolve_scope(cfg: &Config, raw: Option<&str>) -> Result<Scope, BadRequest> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty());
    match raw {
        None if cfg.cluster.enabled => Ok(Scope::Cluster),
        None | Some("local") => Ok(Scope::Local),
        Some("cluster") if cfg.cluster.enabled => Ok(Scope::Cluster),
        Some("cluster") => Ok(Scope::Local),
        Some(id) if id == local_node(cfg) => Ok(Scope::Local),
        Some(id) if cfg.cluster.enabled && cfg.cluster.peers.iter().any(|p| p.node_id == id) => {
            Ok(Scope::Node(id.to_string()))
        }
        Some(_) => Err(BadRequest("未知节点".into())),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeerRequest {
    pub name: String,
    pub query: Query,
}

type PeerFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, MissingNode>> + Send + 'a>>;

/// Fan-out seam: production signs a peer call; tests answer from mocks.
pub(crate) trait Peers: Sync {
    fn fetch<'a>(&'a self, node: &'a str, req: &'a PeerRequest) -> PeerFuture<'a>;
}

pub(crate) fn missing(node: &str, reason: &str, detail: &str) -> MissingNode {
    MissingNode {
        node_id: node.to_string(),
        reason: reason.to_string(),
        detail: Some(detail.to_string()),
    }
}

/// Maps a peer failure to the normative reasons. `detail` is short safe
/// English (no URLs, no secrets).
pub(crate) fn classify(node: &str, error: &PeerCallError) -> MissingNode {
    match error {
        PeerCallError::Unreachable(m)
            if m.contains("超时") || m.to_lowercase().contains("timed out") =>
        {
            missing(node, "timeout", "no response in 3000 ms")
        }
        PeerCallError::Unreachable(_) => missing(node, "unreachable", "connection failed"),
        PeerCallError::Refused(m) if m == "HTTP 401" || m == "HTTP 403" => {
            missing(node, "auth", "signed request rejected")
        }
        PeerCallError::Refused(m) if m == "HTTP 404" => {
            missing(node, "version", "peer has no history API")
        }
        PeerCallError::Refused(_) => missing(node, "error", "invalid peer response"),
    }
}

struct LivePeers<'c> {
    cfg: &'c Config,
}

impl Peers for LivePeers<'_> {
    fn fetch<'a>(&'a self, node: &'a str, req: &'a PeerRequest) -> PeerFuture<'a> {
        Box::pin(async move {
            let call = crate::cluster::peer_call::call::<PeerRequest, Value>(
                self.cfg,
                node,
                routes::HISTORY,
                Some(req),
                PEER_TIMEOUT,
            );
            match tokio::time::timeout(PEER_TIMEOUT, call).await {
                Err(_) => Err(missing(node, "timeout", "no response in 3000 ms")),
                Ok(Err(error)) => Err(classify(node, &error)),
                Ok(Ok(envelope)) if envelope.success => envelope
                    .data
                    .ok_or_else(|| missing(node, "error", "empty peer response")),
                Ok(Ok(_)) => Err(missing(node, "error", "peer refused the query")),
            }
        })
    }
}

/// One node's parts from its own DB, as JSON (same shape peers return).
pub(crate) fn local_parts(
    conn: &rusqlite::Connection,
    req: &PeerRequest,
    node: &str,
    now: i64,
) -> rusqlite::Result<Value> {
    let q = &req.query;
    let value = match req.name.as_str() {
        "summary" => serde_json::to_value(query::summary(conn, q, node, now)?),
        "sessions" => serde_json::to_value(query::sessions(conn, q, now)?),
        "events" => serde_json::to_value(query::events(conn, q)?),
        "stats" => {
            let tier = q.tier.clone().unwrap_or_else(|| "raw".into());
            serde_json::to_value(query::stats(conn, q, &tier, None)?)
        }
        _ => Ok(Value::Null),
    };
    Ok(value.unwrap_or(Value::Null))
}

fn empty_parts(name: &str, node: &str) -> Value {
    match name {
        "summary" => serde_json::to_value(SummaryParts {
            per_node: vec![query::NodeParts {
                node_id: node.into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        "sessions" => serde_json::to_value(SessionsPage::default()),
        "events" => serde_json::to_value(EventsPage::default()),
        _ => serde_json::to_value(StatsParts::default()),
    }
    .unwrap_or(Value::Null)
}

/// This node's own DB, behind the same seam as peers.
struct LocalDb;

impl Peers for LocalDb {
    fn fetch<'a>(&'a self, node: &'a str, req: &'a PeerRequest) -> PeerFuture<'a> {
        Box::pin(async move {
            run_local(req.clone(), node.to_string())
                .await
                .map_err(|e| missing(node, "error", &e))
        })
    }
}

/// Local query on a read-only connection, aborted after 2 s.
async fn run_local(req: PeerRequest, node: String) -> Result<Value, String> {
    let Some(shared) = crate::history::shared() else {
        return Ok(empty_parts(&req.name, &node));
    };
    let path = shared.path.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::task::spawn_blocking(move || {
        let conn = crate::history::schema::open_ro(&path).map_err(|e| e.to_string())?;
        let _ = tx.send(conn.get_interrupt_handle());
        local_parts(&conn, &req, &node, crate::storage::now() as i64).map_err(|e| e.to_string())
    });
    let interrupt = rx.await.ok();
    match tokio::time::timeout(QUERY_TIMEOUT, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("统计查询失败".into()),
        Err(_) => {
            if let Some(handle) = interrupt {
                handle.interrupt();
            }
            Err("统计查询超时".into())
        }
    }
}

/// Key a clamping peer adds to its reply; stripped by the coordinator.
pub(crate) const PEER_CLAMP_KEY: &str = "clamped_to_days";

pub(crate) async fn gather(
    cfg: &Config,
    scope: &Scope,
    peers: &dyn Peers,
    req: &PeerRequest,
    local: &dyn Peers,
    partial: &std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
) -> Result<(Vec<Value>, Vec<MissingNode>), String> {
    let targets: Vec<String> = match scope {
        Scope::Local => Vec::new(),
        Scope::Node(id) => vec![id.clone()],
        Scope::Cluster => cfg
            .cluster
            .peers
            .iter()
            .map(|p| p.node_id.clone())
            .filter(|id| *id != cfg.cluster.node_id)
            .collect(),
    };
    let remote = futures_util::future::join_all(targets.iter().map(|node| peers.fetch(node, req)));
    let (local_part, remote) = if matches!(scope, Scope::Node(_)) {
        (None, remote.await)
    } else {
        let node = local_node(cfg);
        let (l, r) = futures_util::future::join(local.fetch(&node, req), remote).await;
        (Some(l.map_err(|m| m.detail.unwrap_or(m.reason))?), r)
    };
    let mut parts: Vec<Value> = local_part.into_iter().collect();
    let mut missing_nodes = Vec::new();
    for (node, result) in targets.iter().zip(remote) {
        match result {
            Ok(mut v) => {
                // A peer with a smaller rollup_days answered a clamped range:
                // it is partial, not missing (design §11, e3797697).
                let clamp = v
                    .as_object_mut()
                    .and_then(|m| m.remove(PEER_CLAMP_KEY))
                    .and_then(|d| d.as_u64());
                if let Some(days) = clamp {
                    let mut partial = partial.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = partial.entry(node.clone()).or_insert(days);
                    *entry = (*entry).min(days);
                }
                parts.push(v)
            }
            Err(m) => missing_nodes.push(m),
        }
    }
    Ok((parts, missing_nodes))
}

fn decode<T: serde::de::DeserializeOwned>(
    parts: Vec<Value>,
    missing_nodes: &mut Vec<MissingNode>,
) -> Vec<T> {
    parts
        .into_iter()
        .filter_map(|v| match serde_json::from_value(v) {
            Ok(t) => Some(t),
            Err(_) => {
                missing_nodes.push(missing("unknown", "error", "invalid peer response"));
                None
            }
        })
        .collect()
}

/// Shared request handling for the four data routes.
pub(crate) async fn answer(
    cfg: &Config,
    name: &str,
    params: &HashMap<String, String>,
    peers: &dyn Peers,
    local: &dyn Peers,
    now: i64,
) -> Result<Value, (StatusCode, String)> {
    let bad = |e: BadRequest| (StatusCode::BAD_REQUEST, e.0);
    let retention = cfg.history.retention.clone();
    let default_limit = if name == "sessions" { 50 } else { 20 };
    let mut q = query::parse(params, now, retention.rollup_days, default_limit).map_err(bad)?;
    let mut scope = resolve_scope(cfg, params.get("scope").map(String::as_str)).map_err(bad)?;
    // Design §11: only when `parse` really cut the range; a session-scoped
    // stats query takes its range from the session instead.
    let clamped = if name == "stats" && q.session.is_some() {
        None
    } else {
        q.clamped_to_days
    };
    let partial = std::sync::Mutex::new(std::collections::BTreeMap::new());
    let sources = Sources {
        peers,
        local,
        partial: &partial,
    };
    let mut out = answer_inner(cfg, name, &mut q, &mut scope, &sources, now).await?;
    if let Some(map) = out.as_object_mut() {
        if let Some(days) = clamped {
            map.insert("clamped_to_days".into(), json!(days));
        }
        let partial = partial.into_inner().unwrap_or_else(|e| e.into_inner());
        if !partial.is_empty() {
            let nodes: Vec<Value> = partial
                .into_iter()
                .map(|(node, days)| json!({"node": node, "clamped_to_days": days}))
                .collect();
            map.insert("partial_nodes".into(), Value::Array(nodes));
        }
    }
    Ok(out)
}

/// Where an answer's parts come from, plus which peers clamped.
struct Sources<'a> {
    peers: &'a dyn Peers,
    local: &'a dyn Peers,
    partial: &'a std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
}

async fn answer_inner(
    cfg: &Config,
    name: &str,
    q: &mut Query,
    scope: &mut Scope,
    sources: &Sources<'_>,
    now: i64,
) -> Result<Value, (StatusCode, String)> {
    let Sources {
        peers,
        local,
        partial,
    } = *sources;
    let internal = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, e);
    let retention = cfg.history.retention.clone();
    let node = local_node(cfg);
    let run = |req: PeerRequest, scope: Scope| async move {
        gather(cfg, &scope, peers, &req, local, partial).await
    };
    match name {
        "summary" => {
            let req = PeerRequest {
                name: name.into(),
                query: q.clone(),
            };
            let (parts, mut missing_nodes) = run(req, scope.clone()).await.map_err(internal)?;
            let parts: Vec<SummaryParts> = decode(parts, &mut missing_nodes);
            Ok(query::render_summary(
                &query::merge_summary(parts),
                &missing_nodes,
            ))
        }
        "sessions" => {
            let limit = q.limit;
            let req = PeerRequest {
                name: name.into(),
                query: q.clone(),
            };
            let (parts, mut missing_nodes) = run(req, scope.clone()).await.map_err(internal)?;
            let mut pages: Vec<SessionsPage> = decode(parts, &mut missing_nodes);
            if q.id.is_some() && *scope == Scope::Cluster {
                let target = pages.iter().flat_map(|p| p.items.iter()).next().cloned();
                if let Some(target) = target {
                    let mut around = q.clone();
                    around.id = None;
                    around.around = Some((target.started_at, target.eid.clone()));
                    let req = PeerRequest {
                        name: name.into(),
                        query: around,
                    };
                    let (more, mut m2) = run(req, scope.clone()).await.map_err(internal)?;
                    pages.extend(decode::<SessionsPage>(more, &mut m2));
                }
            }
            let mut page = query::merge_sessions(pages, limit);
            if q.id.is_none() {
                page.prev_id = None;
                page.next_id = None;
            }
            page.missing_nodes = missing_nodes;
            Ok(serde_json::to_value(page).unwrap_or(Value::Null))
        }
        "events" => {
            let limit = q.limit;
            let req = PeerRequest {
                name: name.into(),
                query: q.clone(),
            };
            let (parts, mut missing_nodes) = run(req, scope.clone()).await.map_err(internal)?;
            let pages: Vec<EventsPage> = decode(parts, &mut missing_nodes);
            let mut page = query::merge_events(pages, limit);
            page.missing_nodes = missing_nodes;
            Ok(serde_json::to_value(page).unwrap_or(Value::Null))
        }
        "stats" => {
            let mut missing_nodes = Vec::new();
            if let Some(id) = q.session.clone() {
                // Resolve the session first; its node owns the samples.
                let mut lookup = q.clone();
                lookup.id = Some(id);
                lookup.session = None;
                let req = PeerRequest {
                    name: "sessions".into(),
                    query: lookup,
                };
                let (parts, mut m) = run(req, scope.clone()).await.map_err(internal)?;
                let pages: Vec<SessionsPage> = decode(parts, &mut m);
                let Some(s) = pages.into_iter().flat_map(|p| p.items).next() else {
                    return Ok(
                        json!({ "resolution": "raw", "ts": [], "series": {}, "missing_nodes": m }),
                    );
                };
                q.from = s.started_at;
                q.to = s.ended_at.unwrap_or(now).max(s.started_at + 1);
                *scope = if s.node == node {
                    Scope::Local
                } else {
                    Scope::Node(s.node.clone())
                };
                missing_nodes.extend(m);
            }
            q.tier = Some(query::choose_tier(q.from, q.to, now, retention.raw_days).into());
            let req = PeerRequest {
                name: name.into(),
                query: q.clone(),
            };
            let (parts, mut m) = run(req, scope.clone()).await.map_err(internal)?;
            missing_nodes.append(&mut m);
            let parts: Vec<StatsParts> = decode(parts, &mut missing_nodes);
            let mut out = match query::merge_stats(parts) {
                Some(p) => query::render_stats(&p),
                None => json!({ "resolution": "raw", "ts": [], "series": {} }),
            };
            out["missing_nodes"] = json!(missing_nodes);
            Ok(out)
        }
        _ => Err((StatusCode::NOT_FOUND, "Unknown route".into())),
    }
}

async fn handle(name: &str, params: HashMap<String, String>) -> Response {
    let Ok(_permit) = QUERIES.try_acquire() else {
        return fail(StatusCode::TOO_MANY_REQUESTS, "统计查询繁忙");
    };
    let cfg = match crate::config::load_config().await {
        Ok(cfg) => cfg,
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "读取配置失败"),
    };
    let peers = LivePeers { cfg: &cfg };
    match answer(
        &cfg,
        name,
        &params,
        &peers,
        &LocalDb,
        crate::storage::now() as i64,
    )
    .await
    {
        Ok(data) => ok(data),
        Err((status, message)) => fail(status, &message),
    }
}

pub async fn summary(Params(params): Params<HashMap<String, String>>) -> Response {
    handle("summary", params).await
}

pub async fn sessions(Params(params): Params<HashMap<String, String>>) -> Response {
    handle("sessions", params).await
}

pub async fn events(Params(params): Params<HashMap<String, String>>) -> Response {
    handle("events", params).await
}

pub async fn stats(Params(params): Params<HashMap<String, String>>) -> Response {
    handle("stats", params).await
}

pub(crate) fn status_value(cfg: &Config) -> Value {
    let retention = &cfg.history.retention;
    let retention = json!({
        "raw_days": retention.raw_days, "rollup_days": retention.rollup_days,
        "event_days": retention.event_days, "event_rows": retention.event_rows,
    });
    let recorder = if !cfg.cluster.enabled
        || crate::cluster::cluster_state_read().active_owner.as_deref()
            == Some(cfg.cluster.node_id.as_str())
    {
        "primary"
    } else {
        "none"
    };
    let Some(shared) = crate::history::shared() else {
        return json!({
            "enabled": false, "schema_version": crate::history::schema::SCHEMA_VERSION, "oldest_at": null,
            "recording_since": null, "db_bytes": 0,
            "rows": {"sessions": 0, "runs": 0, "events": 0, "samples_raw": 0, "samples_1m": 0, "notify_sends": 0},
            "dropped": {"history": 0, "notify": 0},
            "writer": {"state": "stopped", "last_batch_at": null, "last_error": null},
            "retention": retention, "recorder": recorder, "missing_nodes": [],
        });
    };
    let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let wal = shared.path.with_extension("sqlite-wal");
    let (rows, oldest) = match crate::history::schema::open_ro(&shared.path) {
        Ok(conn) => (
            query::status_rows(&conn).unwrap_or(Value::Null),
            query::oldest_at(&conn).ok().and_then(|o| o.0),
        ),
        Err(_) => (Value::Null, None),
    };
    let writer = shared
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    json!({
        "enabled": true,
        "schema_version": crate::history::schema::SCHEMA_VERSION,
        "oldest_at": oldest,
        "recording_since": shared.recording_since,
        "db_bytes": size(&shared.path) + size(&wal),
        "rows": rows,
        "dropped": {"history": shared.dropped.load(std::sync::atomic::Ordering::Relaxed), "notify": 0},
        // Writer errors are SQLite messages; never paths or secrets.
        "writer": {"state": writer.state, "last_batch_at": writer.last_batch_at,
                   "last_error": writer.last_error.map(|e| crate::logging::safe_text(&e, 120))},
        "retention": retention,
        "recorder": recorder,
        "missing_nodes": [],
    })
}

pub async fn status() -> Response {
    let cfg = match crate::config::load_config().await {
        Ok(cfg) => cfg,
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "读取配置失败"),
    };
    let value = tokio::task::spawn_blocking(move || status_value(&cfg))
        .await
        .unwrap_or(Value::Null);
    ok(value)
}

/// Signed peer route: this node's own parts, never fanned out again.
/// Design §11: read-only (a read-only connection, no writes anywhere),
/// members only (the peer-auth layer answers 401 for a stale or unknown
/// signer before this runs), and any body that is not a valid history
/// query is a 400 before the database is opened.
pub async fn peer_history(
    body: Result<Json<PeerRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(mut req)) = body else {
        return fail(StatusCode::BAD_REQUEST, "统计查询无效");
    };
    let cfg = match crate::config::load_config().await {
        Ok(cfg) => cfg,
        Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "读取配置失败"),
    };
    let clamped = match check_peer_request(&mut req, &cfg, crate::storage::now() as i64) {
        Ok(c) => c,
        Err(e) => return fail(StatusCode::BAD_REQUEST, &e.0),
    };
    let Ok(_permit) = QUERIES.try_acquire() else {
        return fail(StatusCode::TOO_MANY_REQUESTS, "统计查询繁忙");
    };
    match run_local(req, local_node(&cfg)).await {
        Ok(v) => ok(with_peer_clamp(v, clamped)),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// The peer route's whole admission rule for the body. A range longer than
/// this node's `rollup_days` is clamped to it (design §11, e3797697); the
/// returned value is how far, for the reply.
pub(crate) fn check_peer_request(
    req: &mut PeerRequest,
    cfg: &Config,
    now: i64,
) -> Result<Option<u64>, BadRequest> {
    if !matches!(
        req.name.as_str(),
        "summary" | "sessions" | "events" | "stats"
    ) {
        return Err(BadRequest("未知统计查询".into()));
    }
    crate::history::query::validate_peer(&req.query, now)
        .map_err(|e| BadRequest(format!("统计查询无效: {}", e.0)))?;
    Ok(crate::history::query::clamp_peer(
        &mut req.query,
        cfg.history.retention.rollup_days,
    ))
}

/// Adds the clamp marker to a peer reply.
pub(crate) fn with_peer_clamp(mut v: Value, clamped: Option<u64>) -> Value {
    if let (Some(days), Some(map)) = (clamped, v.as_object_mut()) {
        map.insert(PEER_CLAMP_KEY.into(), json!(days));
    }
    v
}
