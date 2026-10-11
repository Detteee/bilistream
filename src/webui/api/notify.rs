//! `/api/notify*`: QQ 通知 settings, status, test send and openid binding.
//!
//! Settings live in their own store records (`notify-config`,
//! `notify-secrets`) with their own revisions, so saving here never makes
//! `/api/config` answer 409, and the other way round.

use super::ApiResponse;
use crate::notify::{self, bind::BindMode, config::validate, store, Command, NotifyConfig};
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

fn forbidden_origin() -> Response {
    reply(
        StatusCode::FORBIDDEN,
        None,
        "请求来源或登录状态已失效，请刷新页面",
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

fn settings_view(store: &crate::storage::Store) -> std::io::Result<Value> {
    let (config, revision) = store::load_config(store)?;
    let (secret, secret_revision) = store::load_secret(store)?;
    Ok(json!({
        "config": config,
        "revision": revision.unwrap_or(0),
        "app_secret_configured": secret.is_some(),
        "secret_revision": secret_revision.unwrap_or(0),
    }))
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

/// Apply a settings update. Separate from the handler for tests.
pub fn apply_update(
    store: &crate::storage::Store,
    update: UpdateNotify,
) -> Result<Value, (StatusCode, String)> {
    let conflict = |error: std::io::Error| (io_status(&error), error.to_string());
    let secret = update
        .app_secret
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty());
    if secret.is_some_and(|secret| {
        secret.len() > store::MAX_SECRET_LEN || secret.chars().any(char::is_control)
    }) {
        return Err((StatusCode::BAD_REQUEST, "AppSecret 格式无效".to_string()));
    }
    let mut next: Option<NotifyConfig> = None;
    if let Some(patch) = update.config.as_ref().filter(|patch| !patch.is_null()) {
        let (current, revision) = store::load_config(store).map_err(conflict)?;
        if update.expected_revision.unwrap_or(0) != revision.unwrap_or(0) {
            return Err((
                StatusCode::CONFLICT,
                "配置已被其他操作修改，请重新加载后再保存".to_string(),
            ));
        }
        let mut value = serde_json::to_value(&current).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "通知设置无法读取".to_string(),
            )
        })?;
        merge(&mut value, patch);
        let mut parsed: NotifyConfig = serde_json::from_value(value)
            .map_err(|_| (StatusCode::BAD_REQUEST, "通知设置格式无效".to_string()))?;
        parsed.qq.app_id = parsed.qq.app_id.trim().to_string();
        parsed.qq.admin_openid = parsed.qq.admin_openid.trim().to_string();
        validate(&parsed).map_err(|message| (StatusCode::BAD_REQUEST, message))?;
        next = Some(parsed);
    }
    if secret.is_some() || update.clear_app_secret {
        let (_, revision) = store::load_secret(store).map_err(conflict)?;
        if update.expected_secret_revision.unwrap_or(0) != revision.unwrap_or(0) {
            return Err((
                StatusCode::CONFLICT,
                "配置已被其他操作修改，请重新加载后再保存".to_string(),
            ));
        }
    }
    if let Some(cfg) = next.as_ref() {
        store::save_config(store, cfg, update.expected_revision).map_err(conflict)?;
    }
    if let Some(secret) = secret {
        store::save_secret(store, Some(secret), update.expected_secret_revision)
            .map_err(conflict)?;
    } else if update.clear_app_secret {
        store::save_secret(store, None, update.expected_secret_revision).map_err(conflict)?;
    }
    settings_view(store).map_err(conflict)
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
    match apply_update(&store, update) {
        Ok(view) => {
            let _ = notify::send_command(Command::Reload);
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
        Err((status, message)) => reply(status, None, message),
    }
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
        let nodes: Vec<Value> = if digest.enabled {
            digest
                .nodes
                .iter()
                .map(|node| {
                    let local = node.id == digest.local;
                    json!({
                        "node_id": node.id,
                        "is_local": local,
                        "notify_capable": if local { status["capable"].as_bool().unwrap_or(false) } else { node.notify_capable },
                        "notify_authority": if local { local_authority } else { node.notify_authority && node.fresh },
                        "fresh": node.fresh,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let authority_node = if local_authority || fallback {
            Some(digest.local.clone())
        } else {
            digest
                .nodes
                .iter()
                .find(|node| node.id != digest.local && node.fresh && node.notify_authority)
                .map(|node| node.id.clone())
        };
        if let Some(object) = status.as_object_mut() {
            object.insert("authority_node_id".into(), json!(authority_node));
            object.insert("nodes".into(), json!(nodes));
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
        Ok(Ok(Err(refusal))) => {
            let data = serde_json::to_value(&refusal).ok();
            reply(StatusCode::CONFLICT, data, refusal.reason)
        }
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

    #[test]
    fn update_merges_validates_and_never_echoes_the_secret() {
        let (root, store) = temp_store();
        let view = apply_update(
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
        assert_eq!(view["app_secret_configured"], true);
        assert!(!view.to_string().contains("synthetic-api-secret-marker"));

        // Stale revision → 409; the main config record is untouched.
        let stale = apply_update(
            &store,
            UpdateNotify {
                config: Some(json!({"enabled": false})),
                expected_revision: Some(0),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(stale.0, StatusCode::CONFLICT);
        assert!(store.read("config.json").unwrap().is_none());

        // Out-of-range values are refused server-side.
        let revision = view["revision"].as_u64();
        let bad = apply_update(
            &store,
            UpdateNotify {
                config: Some(json!({"cooldown_secs": 5})),
                expected_revision: revision,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(bad.0, StatusCode::BAD_REQUEST);

        // Clearing the secret.
        let cleared = apply_update(
            &store,
            UpdateNotify {
                clear_app_secret: true,
                expected_secret_revision: view["secret_revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cleared["app_secret_configured"], false);
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
