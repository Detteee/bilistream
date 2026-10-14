//! `/api/notify*`: QQ 通知 settings, status, test send and openid binding.
//!
//! Settings live in their own store records (`notify-config`,
//! `notify-secrets`, `notify-openid`) with their own revisions, so saving
//! here never makes `/api/config` answer 409, and the other way round.
//!
//! The admin openid is write-only: responses carry `openid_set`,
//! `openid_bound_at` and `openid_revision`, never the value. Writes go
//! through the single writer (`notify::check_writer`) and refusals use the
//! contract's 409 body `{success:false, message, data:{code, ...}}`.

use super::ApiResponse;
use crate::notify::{
    self, bind::BindMode, config::validate, store, Command, NotifyConfig, Refusal,
};
use crate::webui::listen::{AuthGeneration, AuthState};
use axum::http::HeaderMap;
use axum::{http::StatusCode, response::IntoResponse, response::Response, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

fn reply(status: StatusCode, data: Option<Value>, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        Json(ApiResponse {
            success: status.is_success(),
            data,
            message: (!message.is_empty()).then_some(message),
        }),
    )
        .into_response()
}

/// Same check as the membership routes: same Origin and the credential
/// generation that admitted the request (no cross-site simple POSTs).
fn mutation_allowed(
    auth: &AuthState,
    headers: &HeaderMap,
    generation: Option<Extension<AuthGeneration>>,
) -> bool {
    auth.browser_mutation_allowed(headers, generation.map(|Extension(g)| g))
}

fn refused(refusal: Refusal) -> Response {
    reply(StatusCode::CONFLICT, Some(refusal.data), refusal.message)
}

fn forbidden_origin() -> Response {
    reply(
        StatusCode::FORBIDDEN,
        None,
        "登录已失效或请求来源无效，请刷新页面",
    )
}

fn io_status(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::WouldBlock => StatusCode::CONFLICT,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn openid_view(openid: &store::OpenidState) -> Value {
    json!({
        "openid_set": openid.is_set(),
        "openid_bound_at": openid.bound_at,
        "openid_revision": openid.revision,
    })
}

fn settings_view(store: &crate::storage::Store) -> std::io::Result<Value> {
    let (config, revision) = store::load_config(store)?;
    let (secret, secret_revision) = store::load_secret(store)?;
    let openid = store::load_openid(store)?;
    let mut view = json!({
        "config": config,
        "revision": revision.unwrap_or(0),
        "appsecret_set": secret.is_some(),
        // Deprecated duplicate of `appsecret_set` (panel batch 2 drops it).
        "app_secret_configured": secret.is_some(),
        "secret_revision": secret_revision.unwrap_or(0),
    });
    if let (Some(view), Value::Object(extra)) = (view.as_object_mut(), openid_view(&openid)) {
        view.extend(extra);
    }
    Ok(view)
}

#[cfg(test)]
pub(crate) fn settings_view_for_tests(store: &crate::storage::Store) -> std::io::Result<Value> {
    settings_view(store)
}

/// Channels the 关注列表 picker offers: `channels.json` plus the channels in
/// the current YouTube/Twitch/Niconico settings. Deduped by identity.
pub fn watch_candidates(cfg: Option<&crate::config::Config>) -> Vec<Value> {
    use crate::notify::config::WatchChannel;
    let mut out: Vec<WatchChannel> = Vec::new();
    let mut push = |platform: &str, id: Option<&str>, name: &str| {
        let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let channel = WatchChannel {
            platform: platform.to_string(),
            channel_id: id.to_string(),
            name: name.trim().to_string(),
        };
        if !out.iter().any(|known| known.ident() == channel.ident()) {
            out.push(channel);
        }
    };
    if let Ok(channels) = crate::config::load_channels() {
        for channel in &channels.channels {
            push(
                "youtube",
                channel.platforms.youtube.as_deref(),
                &channel.name,
            );
            push("twitch", channel.platforms.twitch.as_deref(), &channel.name);
            push(
                "niconico",
                channel.platforms.niconico.as_deref(),
                channel.niconico_name.as_deref().unwrap_or(&channel.name),
            );
        }
    }
    if let Some(cfg) = cfg {
        push(
            "youtube",
            Some(&cfg.youtube.channel_id),
            &cfg.youtube.channel_name,
        );
        push(
            "twitch",
            Some(&cfg.twitch.channel_id),
            &cfg.twitch.channel_name,
        );
        push(
            "niconico",
            Some(&cfg.niconico.channel_id),
            &cfg.niconico.channel_name,
        );
    }
    out.iter()
        .filter(|channel| crate::notify::config::watch_channel_valid(channel))
        .map(|channel| json!(channel))
        .collect()
}

pub async fn get_notify() -> Response {
    let cfg = crate::config::load_config().await.ok();
    let result = crate::storage::global().and_then(|store| settings_view(&store));
    match result {
        Ok(mut view) => {
            view["watch_candidates"] = json!(watch_candidates(cfg.as_ref()));
            if let Some(cfg) = cfg.as_ref() {
                let status = notify::status_view();
                let digest = notify::authority::read_digest(cfg, crate::storage::now());
                let local_authority = matches!(
                    status.get("authority").and_then(Value::as_str),
                    Some("primary" | "delegate")
                );
                if let (_, Some(set)) = readiness_view(&status, &digest, local_authority) {
                    view["writer_openid_set"] = json!(set);
                }
            }
            reply(StatusCode::OK, Some(view), "")
        }
        Err(error) => reply(io_status(&error), None, error.to_string()),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UpdateNotify {
    /// Partial `NotifyConfig` (JSON merge patch onto the saved value).
    pub config: Option<Value>,
    /// The `revision` from GET; omitted means "no config change".
    pub expected_revision: Option<u64>,
    /// New AppSecret; empty or missing leaves it unchanged.
    pub app_secret: Option<String>,
    pub clear_app_secret: bool,
    pub expected_secret_revision: Option<u64>,
    /// New admin openid (write-only); empty or missing leaves it unchanged.
    pub admin_openid: Option<String>,
    /// The `openid_revision` from GET; required with `admin_openid`.
    pub expected_openid_revision: Option<u64>,
}

impl UpdateNotify {
    /// Wipe the write-only values once the update was applied or refused.
    fn wipe(&mut self) {
        if let Some(secret) = self.app_secret.as_mut() {
            zeroize::Zeroize::zeroize(secret);
        }
        if let Some(openid) = self.admin_openid.as_mut() {
            zeroize::Zeroize::zeroize(openid);
        }
    }
}

/// Why an update was not applied.
#[derive(Debug)]
pub enum UpdateError {
    /// 400 / 500 with `data: null`.
    Status(StatusCode, String),
    /// 409 with `data.code`.
    Refused(Refusal),
}

impl UpdateError {
    fn into_response(self) -> Response {
        match self {
            UpdateError::Status(status, message) => reply(status, None, message),
            UpdateError::Refused(refusal) => refused(refusal),
        }
    }

    #[cfg(test)]
    fn status(&self) -> StatusCode {
        match self {
            UpdateError::Status(status, _) => *status,
            UpdateError::Refused(_) => StatusCode::CONFLICT,
        }
    }
}

/// Store errors: a stale revision is a `revision_conflict` refusal.
fn store_error(error: std::io::Error) -> UpdateError {
    match store::revision_conflict(&error) {
        Some(conflict) => UpdateError::Refused(Refusal::revision_conflict(conflict.current)),
        None => UpdateError::Status(io_status(&error), error.to_string()),
    }
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.insert(key.clone(), Value::Null);
                } else {
                    merge(target.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

/// Apply a settings update. Separate from the handler for tests; `writer`
/// is the single-writer check, consulted only when the body sets the openid.
///
/// Check order (contract): body validation (400), writer (409 `no_writer` /
/// `not_owner`), then revisions (409 `revision_conflict`). Config, AppSecret
/// and openid then land in ONE store transaction (review L7): all or none.
pub fn apply_update(
    store: &crate::storage::Store,
    update: &UpdateNotify,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
) -> Result<Value, UpdateError> {
    let bad = |message: &str| UpdateError::Status(StatusCode::BAD_REQUEST, message.to_string());
    let secret = update
        .app_secret
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty());
    if secret.is_some_and(|secret| {
        secret.len() > store::MAX_SECRET_LEN || secret.chars().any(char::is_control)
    }) {
        return Err(bad("AppSecret 格式无效"));
    }
    let patch = update.config.as_ref().filter(|patch| !patch.is_null());
    if patch.is_some_and(|patch| patch.pointer("/qq/admin_openid").is_some()) {
        // The old unsealed path; the value is never echoed.
        return Err(bad("请在管理员 openid 输入框中填写"));
    }
    let openid = update
        .admin_openid
        .as_deref()
        .map(str::trim)
        .filter(|openid| !openid.is_empty());
    if let Some(openid) = openid {
        notify::config::validate_openid(openid).map_err(|message| bad(&message))?;
        if update.expected_openid_revision.is_none() {
            return Err(bad("缺少 expected_openid_revision"));
        }
    }
    let mut next: Option<(NotifyConfig, u64)> = None;
    if let Some(patch) = patch {
        let (current, _) = store::load_config(store).map_err(store_error)?;
        let mut value = serde_json::to_value(&current).map_err(|_| {
            UpdateError::Status(
                StatusCode::INTERNAL_SERVER_ERROR,
                "通知设置无法读取".to_string(),
            )
        })?;
        merge(&mut value, patch);
        let mut parsed: NotifyConfig =
            serde_json::from_value(value).map_err(|_| bad("通知设置格式无效"))?;
        parsed.qq.app_id = parsed.qq.app_id.trim().to_string();
        validate(&parsed).map_err(|message| bad(&message))?;
        // Compared inside the transaction: a stale revision is a 409.
        next = Some((parsed, update.expected_revision.unwrap_or(0)));
    }
    if openid.is_some() {
        writer().map_err(UpdateError::Refused)?;
    }
    let secret_change = match secret {
        Some(secret) => store::SecretChange::Set(zeroize::Zeroizing::new(secret.to_string())),
        None if update.clear_app_secret => store::SecretChange::Clear,
        None => store::SecretChange::Keep,
    };
    let openid_write = openid.map(|openid| store::OpenidWrite {
        change: store::OpenidChange::Set {
            value: zeroize::Zeroizing::new(openid.to_string()),
            via: "manual",
        },
        expected: update.expected_openid_revision,
        writer: node_id.to_string(),
        now: crate::storage::now(),
    });
    store::save_settings(
        store,
        next.as_ref().map(|(cfg, expected)| (cfg, *expected)),
        secret_change,
        update.expected_secret_revision.unwrap_or(0),
        openid_write,
    )
    .map_err(store_error)?;
    settings_view(store).map_err(store_error)
}

/// This node's id, as the openid writer records it.
async fn local_node_id() -> String {
    crate::config::load_config()
        .await
        .ok()
        .map(|cfg| cfg.cluster.node_id)
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

pub(crate) async fn update_notify(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    Json(update): Json<UpdateNotify>,
) -> Response {
    if !mutation_allowed(&auth, &headers, generation) {
        return forbidden_origin();
    }
    let store = match crate::storage::global() {
        Ok(store) => store,
        Err(error) => return reply(io_status(&error), None, error.to_string()),
    };
    let cleared = update.clear_app_secret
        && update
            .app_secret
            .as_deref()
            .is_none_or(|s| s.trim().is_empty());
    let node_id = local_node_id().await;
    let openid_changed = update
        .admin_openid
        .as_deref()
        .is_some_and(|openid| !openid.trim().is_empty());
    let push_store = Arc::clone(&store);
    let push_node = node_id.clone();
    let mut update = update;
    let result = tokio::task::spawn_blocking(move || {
        let result = apply_update(&store, &update, &node_id, &|| {
            notify::check_writer("修改管理员 openid")
        });
        update.wipe();
        result
    })
    .await
    .unwrap_or_else(|_| {
        Err(UpdateError::Status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "通知设置保存失败".to_string(),
        ))
    });
    match result {
        Ok(view) => {
            let _ = notify::send_command(Command::Reload);
            if openid_changed {
                // 1b: the new value goes to every current member.
                notify::openid_sync::spawn_push(push_store, push_node);
            }
            reply(
                StatusCode::OK,
                Some(view),
                if cleared {
                    "AppSecret 已清除"
                } else {
                    "通知设置已保存"
                },
            )
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Deserialize)]
struct UnbindRequest {
    expected_openid_revision: Option<u64>,
}

fn parse_unbind(body: &[u8]) -> Option<u64> {
    serde_json::from_slice::<UnbindRequest>(body)
        .ok()
        .and_then(|request| request.expected_openid_revision)
}

/// Clear the admin openid on the single writer. Separate for tests.
pub fn apply_unbind(
    store: &crate::storage::Store,
    expected: u64,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
) -> Result<(Value, bool), UpdateError> {
    writer().map_err(UpdateError::Refused)?;
    let was_set = store::load_openid(store).map_err(store_error)?.is_set();
    store::write_openid(
        store,
        store::OpenidWrite {
            change: store::OpenidChange::Clear,
            expected: Some(expected),
            writer: node_id.to_string(),
            now: crate::storage::now(),
        },
    )
    .map_err(store_error)?;
    let openid = store::load_openid(store).map_err(store_error)?;
    Ok((openid_view(&openid), was_set))
}

/// `DELETE /api/notify/openid` with `{expected_openid_revision}`: the only
/// unbind. The body is read as bytes so a missing or malformed body still
/// gets the normal `{success, data, message}` wrapper.
pub(crate) async fn notify_unbind(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !mutation_allowed(&auth, &headers, generation) {
        return forbidden_origin();
    }
    let Some(expected) = parse_unbind(&body) else {
        return reply(
            StatusCode::BAD_REQUEST,
            None,
            "缺少 expected_openid_revision",
        );
    };
    let store = match crate::storage::global() {
        Ok(store) => store,
        Err(error) => return reply(io_status(&error), None, error.to_string()),
    };
    let node_id = local_node_id().await;
    let push_store = Arc::clone(&store);
    let push_node = node_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        apply_unbind(&store, expected, &node_id, &|| {
            notify::check_writer("解除绑定")
        })
    })
    .await
    .unwrap_or_else(|_| {
        Err(UpdateError::Status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "解除绑定失败".to_string(),
        ))
    });
    match result {
        Ok((view, was_set)) => {
            let _ = notify::send_command(Command::Reload);
            // 1b: clear it on every current member, retried until acked.
            notify::openid_sync::spawn_push(push_store, push_node);
            reply(
                StatusCode::OK,
                Some(view),
                if was_set {
                    "管理员 openid 已解除绑定"
                } else {
                    "管理员 openid 未绑定"
                },
            )
        }
        Err(error) => error.into_response(),
    }
}

fn peer_refusal(refusal: notify::openid_sync::ServeRefusal) -> Response {
    use notify::openid_sync::ServeRefusal;
    let (status, message) = match refusal {
        ServeRefusal::NotMember => (StatusCode::FORBIDDEN, "节点不在当前成员清单"),
        ServeRefusal::Invalid => (StatusCode::BAD_REQUEST, "管理员 openid 副本无效"),
        ServeRefusal::Store => (StatusCode::INTERNAL_SERVER_ERROR, "管理员 openid 无法保存"),
    };
    reply(status, None, message)
}

/// The signed caller's node id and this node's current member set, or the
/// refusal. Without the router's `AuthenticatedNode` (never a browser:
/// the path is peer-only) the call is refused.
fn peer_context(
    peer: Option<Extension<crate::cluster::peer_call::AuthenticatedNode>>,
    headers: &HeaderMap,
) -> Result<(String, Arc<crate::storage::Store>, Vec<String>), Box<Response>> {
    if !notify::openid_sync::inbound_transport_allowed(headers) {
        return Err(Box::new(reply(
            StatusCode::FORBIDDEN,
            None,
            "管理员 openid 只能通过 HTTPS 或本机认证隧道传输",
        )));
    }
    let Some(Extension(peer)) = peer else {
        return Err(Box::new(peer_refusal(
            notify::openid_sync::ServeRefusal::NotMember,
        )));
    };
    let store = crate::storage::global()
        .map_err(|_| Box::new(peer_refusal(notify::openid_sync::ServeRefusal::Store)))?;
    let (_, members) = notify::openid_sync::current_members(&store)
        .map_err(|_| Box::new(peer_refusal(notify::openid_sync::ServeRefusal::NotMember)))?;
    Ok((peer.node_id, store, members))
}

/// `POST /api/cluster/v1/notify/openid` (peer-only, signed): a member's
/// copy; the greater `(openid_revision, writer)` is kept. Answers
/// `{applied, revision}`, never the value.
pub(crate) async fn peer_openid_push(
    peer: Option<Extension<crate::cluster::peer_call::AuthenticatedNode>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (from, store, members) = match peer_context(peer, &headers) {
        Ok(context) => context,
        Err(response) => return *response,
    };
    let body = zeroize::Zeroizing::new(body.to_vec());
    let result = tokio::task::spawn_blocking(move || {
        notify::openid_sync::serve_push(&store, &from, &members, &body)
    })
    .await
    .unwrap_or(Err(notify::openid_sync::ServeRefusal::Store));
    match result {
        Ok(ack) => {
            if ack.applied {
                let _ = notify::send_command(Command::Reload);
            }
            reply(StatusCode::OK, Some(json!(ack)), "")
        }
        Err(refusal) => peer_refusal(refusal),
    }
}

/// `GET /api/cluster/v1/notify/openid` (peer-only, signed): this node's
/// record for a joining node or a new writer, `unbound` included.
pub(crate) async fn peer_openid_pull(
    peer: Option<Extension<crate::cluster::peer_call::AuthenticatedNode>>,
    headers: HeaderMap,
) -> Response {
    let (from, store, members) = match peer_context(peer, &headers) {
        Ok(context) => context,
        Err(response) => return *response,
    };
    let result = tokio::task::spawn_blocking(move || {
        notify::openid_sync::serve_pull(&store, &from, &members)
    })
    .await
    .unwrap_or(Err(notify::openid_sync::ServeRefusal::Store));
    match result {
        Ok(copy) => {
            let data = serde_json::to_value(&copy).ok();
            reply(StatusCode::OK, data, "")
        }
        Err(refusal) => peer_refusal(refusal),
    }
}

/// `sender: {node, last_send}`: the authority node and its last real send,
/// from this node's ledger joined with fresh peer heartbeats (no reason
/// text, no ids).
fn sender_view(node: Option<&str>, digest: &notify::observer::ClusterDigest, now: u64) -> Value {
    let mut ledger = notify::SHARED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ledger
        .clone();
    for peer in &digest.peer_ledgers {
        let mut peer = peer.clone();
        peer.clamp_future(now);
        ledger.join(&peer);
    }
    let last_send = node.and_then(|node| ledger.last_send.get(node).cloned());
    json!({ "node": node, "last_send": last_send })
}

/// Addendum C fields from the worker status and the heartbeat digest.
fn readiness_view(
    status: &Value,
    digest: &notify::observer::ClusterDigest,
    local_authority: bool,
) -> (Vec<Value>, Option<bool>) {
    let (appsecret, openid) = notify::local_credential_flags();
    let writer = notify::readiness::writer_from_status(status);
    notify::readiness::view(
        digest,
        writer.as_ref(),
        (notify::local_appid_set(), appsecret, openid),
        status["capable"].as_bool().unwrap_or(false),
        local_authority,
    )
}

pub async fn notify_status() -> Response {
    let mut status = notify::status_view();
    if let Ok(cfg) = crate::config::load_config().await {
        let now = crate::storage::now();
        let digest = notify::authority::read_digest(&cfg, now);
        let local_authority = matches!(
            status.get("authority").and_then(Value::as_str),
            Some("primary" | "delegate")
        );
        let fallback =
            status.get("authority").and_then(Value::as_str) == Some("no_quorum_fallback");
        let (nodes, writer_openid) = readiness_view(&status, &digest, local_authority);
        let authority_node = if local_authority || fallback {
            Some(digest.local.clone())
        } else {
            digest
                .nodes
                .iter()
                .find(|node| node.id != digest.local && node.fresh && node.notify_authority)
                .map(|node| node.id.clone())
        };
        let sender = sender_view(authority_node.as_deref(), &digest, now);
        if let Some(object) = status.as_object_mut() {
            object.insert("authority_node_id".into(), json!(authority_node));
            object.insert("nodes".into(), json!(nodes));
            object.remove("writer_openid_set");
            if let Some(set) = writer_openid {
                object.insert("writer_openid_set".into(), json!(set));
            }
            object.insert("sender".into(), sender);
        }
    }
    reply(StatusCode::OK, Some(status), "")
}

pub(crate) async fn notify_test(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
) -> Response {
    if !mutation_allowed(&auth, &headers, generation) {
        return forbidden_origin();
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if let Err(message) = notify::send_command(Command::Test { reply: sender }) {
        return reply(StatusCode::SERVICE_UNAVAILABLE, None, message);
    }
    match tokio::time::timeout(Duration::from_secs(45), receiver).await {
        Ok(Ok(report)) => {
            let message = if report.ok {
                "测试消息已发送".to_string()
            } else {
                format!(
                    "测试消息发送失败：{}",
                    report.reason.clone().unwrap_or_default()
                )
            };
            let data = serde_json::to_value(&report).ok();
            // The QQ outcome is in `data.ok`; the request itself succeeded.
            reply(StatusCode::OK, data, message)
        }
        _ => reply(StatusCode::GATEWAY_TIMEOUT, None, "测试消息发送超时"),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BindRequest {
    pub mode: BindMode,
    /// Confirmed in the panel: replace an already bound openid.
    pub replace: bool,
}

/// `Json<…>` (not `Option`): the body must be `application/json`, which a
/// cross-site form cannot send without a preflight.
pub(crate) async fn notify_bind(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
    Json(body): Json<BindRequest>,
) -> Response {
    if !mutation_allowed(&auth, &headers, generation) {
        return forbidden_origin();
    }
    if !notify::worker_running() {
        // Unknown writer: the server refuses the openid write (contract).
        return refused(Refusal::no_writer("绑定"));
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if let Err(message) = notify::send_command(Command::StartBind {
        mode: body.mode,
        replace: body.replace,
        reply: sender,
    }) {
        return reply(StatusCode::SERVICE_UNAVAILABLE, None, message);
    }
    // Above the worker's 25 s send cap, so a busy worker still answers.
    match tokio::time::timeout(Duration::from_secs(30), receiver).await {
        Ok(Ok(Ok(view))) => reply(StatusCode::OK, Some(view), "请在 QQ 向机器人发送此码"),
        Ok(Ok(Err(refusal))) => refused(refusal),
        _ => reply(StatusCode::GATEWAY_TIMEOUT, None, "通知服务无响应"),
    }
}

pub(crate) async fn notify_cancel_bind(
    Extension(auth): Extension<Arc<AuthState>>,
    generation: Option<Extension<AuthGeneration>>,
    headers: HeaderMap,
) -> Response {
    if !mutation_allowed(&auth, &headers, generation) {
        return forbidden_origin();
    }
    match notify::send_command(Command::CancelBind) {
        Ok(()) => reply(StatusCode::OK, None, "已取消绑定"),
        Err(message) => reply(StatusCode::SERVICE_UNAVAILABLE, None, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (std::path::PathBuf, std::sync::Arc<crate::storage::Store>) {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let root = std::env::temp_dir().join(format!(
            "bilistream-notify-api-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        std::fs::create_dir_all(root.join("legacy")).unwrap();
        let store = crate::storage::Store::open(
            root.join("data"),
            root.join("keys/master.key"),
            Some(root.join("legacy")),
        )
        .unwrap();
        (root, store)
    }

    const OPENID: &str = "SAMPLE0OPENID0VALUE0ABCDEF012345";

    fn writer_ok() -> Result<(), Refusal> {
        Ok(())
    }

    fn update(store: &crate::storage::Store, update: UpdateNotify) -> Result<Value, UpdateError> {
        apply_update(store, &update, "solo", &writer_ok)
    }

    fn refusal(error: UpdateError) -> Refusal {
        match error {
            UpdateError::Refused(refusal) => refusal,
            UpdateError::Status(status, message) => panic!("{status}: {message}"),
        }
    }

    #[test]
    fn update_merges_validates_and_never_echoes_the_secret() {
        let (root, store) = temp_store();
        let view = update(
            &store,
            UpdateNotify {
                config: Some(
                    json!({"enabled": true, "qq": {"app_id": " 1234567890 "}, "confirm_secs": 90}),
                ),
                expected_revision: Some(0),
                app_secret: Some("synthetic-api-secret-marker".into()),
                expected_secret_revision: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view["config"]["qq"]["app_id"], "1234567890");
        assert_eq!(view["config"]["confirm_secs"], 90);
        assert_eq!(view["config"]["cooldown_secs"], 1800);
        assert_eq!(view["appsecret_set"], true);
        assert_eq!(view["app_secret_configured"], true);
        assert_eq!(view["openid_set"], false);
        assert_eq!(view["openid_revision"], 0);
        assert!(view["openid_bound_at"].is_null());
        assert!(view["config"]["qq"].get("admin_openid").is_none());
        assert!(!view.to_string().contains("synthetic-api-secret-marker"));

        // Stale revision → 409 revision_conflict with the current revision;
        // the main config record is untouched.
        let stale = update(
            &store,
            UpdateNotify {
                config: Some(json!({"enabled": false})),
                expected_revision: Some(0),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        let stale = refusal(stale);
        assert_eq!(
            stale.data,
            json!({"code": "revision_conflict", "current_revision": view["revision"]})
        );
        assert!(store.read("config.json").unwrap().is_none());

        // Out-of-range values are refused server-side.
        let revision = view["revision"].as_u64();
        let bad = update(
            &store,
            UpdateNotify {
                config: Some(json!({"cooldown_secs": 5})),
                expected_revision: revision,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        // A stale secret revision is a revision_conflict too (O3).
        let stale_secret = refusal(
            update(
                &store,
                UpdateNotify {
                    app_secret: Some("another-secret".into()),
                    expected_secret_revision: Some(0),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        );
        assert_eq!(stale_secret.code(), "revision_conflict");
        assert_eq!(
            stale_secret.data["current_revision"],
            view["secret_revision"]
        );

        // Clearing the secret.
        let cleared = update(
            &store,
            UpdateNotify {
                clear_app_secret: true,
                expected_secret_revision: view["secret_revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cleared["appsecret_set"], false);
        assert_eq!(cleared["app_secret_configured"], false);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn openid_is_write_only_and_validated_before_anything_is_saved() {
        let (root, store) = temp_store();
        let bad = |body: UpdateNotify| match update(&store, body).unwrap_err() {
            UpdateError::Status(status, message) => (status, message),
            UpdateError::Refused(refusal) => panic!("{refusal:?}"),
        };
        // The old nested path is refused (and the value never echoed).
        let (status, message) = bad(UpdateNotify {
            config: Some(json!({"qq": {"admin_openid": OPENID}})),
            expected_revision: Some(0),
            ..Default::default()
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(message, "请在管理员 openid 输入框中填写");
        // Even an empty nested value: the field no longer exists.
        let (status, _) = bad(UpdateNotify {
            config: Some(json!({"qq": {"admin_openid": ""}})),
            expected_revision: Some(0),
            ..Default::default()
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, message) = bad(UpdateNotify {
            admin_openid: Some("not an id; DROP".into()),
            expected_openid_revision: Some(0),
            ..Default::default()
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!message.contains("DROP"));
        let (_, message) = bad(UpdateNotify {
            admin_openid: Some(OPENID.into()),
            ..Default::default()
        });
        assert_eq!(message, "缺少 expected_openid_revision");
        assert!(store.read(store::OPENID_RECORD).unwrap().is_none());
        assert!(store.read(store::CONFIG_RECORD).unwrap().is_none());

        // On the writer: saved sealed, response has booleans only.
        let view = update(
            &store,
            UpdateNotify {
                config: Some(json!({"enabled": true})),
                expected_revision: Some(0),
                admin_openid: Some(format!(" {OPENID} ")),
                expected_openid_revision: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view["openid_set"], true);
        assert_eq!(view["openid_revision"], 1);
        assert!(view["openid_bound_at"].as_u64().is_some());
        assert_eq!(view["config"]["enabled"], true);
        assert!(!view.to_string().contains(OPENID));
        let loaded = store::load_openid(&store).unwrap();
        assert_eq!(loaded.openid.unwrap().expose(), OPENID);
        // Empty means unchanged.
        let view = update(
            &store,
            UpdateNotify {
                admin_openid: Some("  ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view["openid_revision"], 1);
        // Stale openid revision: revision_conflict, and the config in the
        // same body is not applied either.
        let conflict = refusal(
            update(
                &store,
                UpdateNotify {
                    config: Some(json!({"enabled": false})),
                    expected_revision: view["revision"].as_u64(),
                    admin_openid: Some("OTHER0000000000000000000000000000".into()),
                    expected_openid_revision: Some(0),
                    ..Default::default()
                },
            )
            .unwrap_err(),
        );
        assert_eq!(
            conflict.data,
            json!({"code": "revision_conflict", "current_revision": 1})
        );
        assert!(store::load_config(&store).unwrap().0.enabled);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn openid_writes_on_a_non_writer_are_refused_and_save_nothing() {
        let (root, store) = temp_store();
        let not_owner = || Err(Refusal::not_owner("osaka-2", "修改管理员 openid"));
        let body = UpdateNotify {
            config: Some(json!({"enabled": true})),
            expected_revision: Some(0),
            admin_openid: Some(OPENID.into()),
            expected_openid_revision: Some(0),
            ..Default::default()
        };
        let refused = refusal(apply_update(&store, &body, "tokyo-1", &not_owner).unwrap_err());
        assert_eq!(
            refused.data,
            json!({"code": "not_owner", "owner": "osaka-2"})
        );
        assert!(refused.message.contains("osaka-2"));
        assert!(store.read(store::CONFIG_RECORD).unwrap().is_none());
        assert!(store.read(store::OPENID_RECORD).unwrap().is_none());
        let no_writer = || Err(Refusal::no_writer("修改管理员 openid"));
        let refused = refusal(apply_update(&store, &body, "tokyo-1", &no_writer).unwrap_err());
        assert_eq!(refused.data, json!({"code": "no_writer"}));
        // Config and AppSecret-only saves work on any node.
        let ok = apply_update(
            &store,
            &UpdateNotify {
                config: Some(json!({"enabled": true})),
                expected_revision: Some(0),
                app_secret: Some("synthetic-secret".into()),
                expected_secret_revision: Some(0),
                ..Default::default()
            },
            "tokyo-1",
            &no_writer,
        )
        .unwrap();
        assert_eq!(ok["appsecret_set"], true);
        // Unbind follows the same rule.
        let refused = refusal(apply_unbind(&store, 0, "tokyo-1", &not_owner).unwrap_err());
        assert_eq!(refused.code(), "not_owner");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unbind_needs_the_revision_and_is_idempotent() {
        let (root, store) = temp_store();
        assert_eq!(parse_unbind(b""), None);
        assert_eq!(parse_unbind(b"{}"), None);
        assert_eq!(parse_unbind(b"{\"expected_openid_revision\":\"3\"}"), None);
        assert_eq!(parse_unbind(b"{\"expected_openid_revision\":3}"), Some(3));
        update(
            &store,
            UpdateNotify {
                admin_openid: Some(OPENID.into()),
                expected_openid_revision: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let stale = refusal(apply_unbind(&store, 0, "solo", &writer_ok).unwrap_err());
        assert_eq!(
            stale.data,
            json!({"code": "revision_conflict", "current_revision": 1})
        );
        let (view, was_set) = apply_unbind(&store, 1, "solo", &writer_ok).unwrap();
        assert!(was_set);
        assert_eq!(
            view,
            json!({"openid_set": false, "openid_bound_at": null, "openid_revision": 2})
        );
        let (view, was_set) = apply_unbind(&store, 2, "solo", &writer_ok).unwrap();
        assert!(!was_set);
        assert_eq!(
            view["openid_revision"], 2,
            "an unbound unbind changes nothing"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn mutating_routes_refuse_a_foreign_origin() {
        crate::install_crypto_provider();
        let (root, store) = temp_store();
        let auth = AuthState::open(store, None).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, "127.0.0.1:3150".parse().unwrap());
        headers.insert(
            axum::http::header::ORIGIN,
            "https://evil.example".parse().unwrap(),
        );
        let generation = || Some(Extension(AuthGeneration(None)));
        let response = notify_bind(
            Extension(Arc::clone(&auth)),
            generation(),
            headers.clone(),
            Json(BindRequest::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response =
            notify_test(Extension(Arc::clone(&auth)), generation(), headers.clone()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response =
            notify_cancel_bind(Extension(Arc::clone(&auth)), generation(), headers.clone()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = notify_unbind(
            Extension(Arc::clone(&auth)),
            generation(),
            headers.clone(),
            axum::body::Bytes::from_static(b"{\"expected_openid_revision\":1}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = update_notify(
            Extension(Arc::clone(&auth)),
            generation(),
            headers.clone(),
            Json(UpdateNotify::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // No Origin at all (a non-browser or a form without one): refused too.
        headers.remove(axum::http::header::ORIGIN);
        let response =
            notify_cancel_bind(Extension(Arc::clone(&auth)), generation(), headers).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(root);
    }
}
