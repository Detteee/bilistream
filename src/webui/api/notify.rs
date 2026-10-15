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
    self, bind::BindMode, config::validate, openid_sync::PeerLink, store, Command, NotifyConfig,
    Refusal,
};
use crate::webui::listen::{AuthGeneration, AuthState};
use axum::http::HeaderMap;
use axum::{http::StatusCode, response::IntoResponse, response::Response, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[path = "notify_forward.rs"]
pub(crate) mod forwarded;

/// Stable code on a 503 that carries none of its own (never a bare 503).
pub const UNAVAILABLE_CODE: &str = "notify_unavailable";

fn reply(status: StatusCode, data: Option<Value>, message: impl Into<String>) -> Response {
    let message = message.into();
    let data = if status == StatusCode::SERVICE_UNAVAILABLE {
        match data {
            Some(Value::Object(mut map)) => {
                map.entry("code").or_insert_with(|| json!(UNAVAILABLE_CODE));
                Some(Value::Object(map))
            }
            None => Some(json!({ "code": UNAVAILABLE_CODE })),
            other => other,
        }
    } else {
        data
    };
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

/// F rev 3 rule 16: the last 4 characters of the AppID, the only part of
/// it ever returned. `None` when unset, and also for an AppID of 4
/// characters or fewer, where "the last 4" would be the whole value.
pub fn app_id_hint(appid: &str) -> Option<String> {
    let chars: Vec<char> = appid.trim().chars().collect();
    (chars.len() > 4).then(|| chars[chars.len() - 4..].iter().collect())
}

fn settings_view(store: &crate::storage::Store) -> std::io::Result<Value> {
    let settings = store::load_settings(store)?;
    let credentials_view = notify::credentials::view_settings(&settings);
    let (config, revision) = settings.config;
    let (secret, secret_revision) = settings.secret;
    let openid = settings.openid;
    let catalog = notify::config::WatchCatalog::load(store)?;
    // F rule 16: the AppID is write-only like the AppSecret. The config
    // goes out with `qq.appid_set` and never with `qq.app_id`.
    let appid = config.qq.app_id.trim().to_string();
    let appid_set = !appid.is_empty();
    let mut config =
        serde_json::to_value(&config).map_err(|_| std::io::Error::other("通知设置无法读取"))?;
    if let Some(qq) = config.get_mut("qq").and_then(Value::as_object_mut) {
        qq.remove("app_id");
        qq.insert("appid_set".into(), json!(appid_set));
        qq.insert("app_id_hint".into(), json!(app_id_hint(&appid)));
    }
    if let Some(entries) = config
        .pointer_mut("/watch/channels")
        .and_then(Value::as_array_mut)
    {
        for entry in entries {
            if entry["kind"] == "configured" {
                let unresolved = entry["name"]
                    .as_str()
                    .is_none_or(|name| !catalog.channels.contains_key(name));
                if unresolved {
                    entry["unresolved"] = json!(true);
                }
            }
        }
    }
    let mut view = json!({
        "config": config,
        "configured_channels": catalog.view(),
        "revision": revision.unwrap_or(0),
        "appsecret_set": secret.is_some(),
        // Deprecated duplicate of `appsecret_set` (panel batch 2 drops it).
        "app_secret_configured": secret.is_some(),
        "secret_revision": secret_revision.unwrap_or(0),
    });
    if let (Some(view), Value::Object(extra)) = (view.as_object_mut(), openid_view(&openid)) {
        view.extend(extra);
    }
    // Addendum F: generation and flags only, never a value.
    if let (Some(view), Value::Object(extra)) = (view.as_object_mut(), credentials_view) {
        view.extend(extra);
    }
    view["watch_revision"] = json!(settings.watch.revision);
    if view["openid_rebind"] == json!(true) {
        view["openid_set"] = json!(false);
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
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |platform: &str, id: Option<&str>, name: &str| {
        let Some(id) = id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        if notify::config::watch_account_valid(platform, id)
            && seen.insert(notify::config::watch_account_ident(platform, id))
        {
            out.push(json!({"platform":platform,"channel_id":id,"name":name.trim()}));
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
    out
}

/// Addendum A/C/D read-only fields on GET /api/notify, copied by name from
/// the worker status (never the ledger itself). `config.live_events_enabled`
/// is always present: `NotifyConfig` serializes every field.
pub(crate) fn live_events_view(view: &mut Value, status: &Value) {
    let pending = status
        .get("pending_live_events")
        .cloned()
        .filter(Value::is_array)
        .unwrap_or_else(|| json!([]));
    view["pending_live_events"] = pending;
    let quota = status.get("quota");
    for (key, fallback) in [
        ("wakeup_remaining", json!(0)),
        (
            "wakeup_reserve",
            json!(notify::live_events::WAKEUP_RESERVE_FOR_ALERTS),
        ),
        ("wakeup_next_close_at", Value::Null),
        ("passive_window_open", json!(false)),
    ] {
        view[key] = quota
            .and_then(|quota| quota.get(key))
            .cloned()
            .unwrap_or(fallback);
    }
}

pub async fn get_notify() -> Response {
    let cfg = crate::config::load_config().await.ok();
    let result = crate::storage::global().and_then(|store| settings_view(&store));
    match result {
        Ok(mut view) => {
            view["watch_candidates"] = json!(watch_candidates(cfg.as_ref()));
            live_events_view(&mut view, &notify::status_view());
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
    /// Addendum F: the `credentials_generation` from GET. A stale value is
    /// 409 `credentials_conflict` when the AppID or AppSecret changes.
    pub base_generation: Option<u64>,
    /// The `watch_revision` from GET. Required when `config.watch` is present.
    /// A stale value is 409 `watch_conflict`.
    pub base_watch_revision: Option<u64>,
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
    /// Addendum F refusal (`credentials_conflict` …).
    Credentials(notify::credentials::SaveError),
    /// Channel watch validation, with a stable code and field path.
    Watch(notify::config::WatchError),
}

impl UpdateError {
    fn into_response(self) -> Response {
        match self {
            UpdateError::Status(status, message) => reply(status, None, message),
            UpdateError::Refused(refusal) => refused(refusal),
            UpdateError::Credentials(error) => credentials_refused(error),
            UpdateError::Watch(error) => reply(
                StatusCode::BAD_REQUEST,
                Some(json!({"code":error.code,"field":error.field})),
                error.message,
            ),
        }
    }

    #[cfg(test)]
    fn status(&self) -> StatusCode {
        match self {
            UpdateError::Status(status, _) => *status,
            UpdateError::Refused(_) => StatusCode::CONFLICT,
            UpdateError::Credentials(error) => credentials_status(*error),
            UpdateError::Watch(_) => StatusCode::BAD_REQUEST,
        }
    }
}

/// Store errors: a stale revision is a `revision_conflict` refusal.
fn store_error(error: std::io::Error) -> UpdateError {
    if let Some(failed) = notify::credentials::save_failed(&error) {
        return UpdateError::Credentials(failed);
    }
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

/// Shared by local and forwarded saves. Normalize legacy follows before
/// validation, strip read-only metadata, and acknowledge migration only
/// when this request actually saves watch preferences.
pub(crate) fn updated_config(
    store: &crate::storage::Store,
    current: &NotifyConfig,
    patch: &Value,
) -> Result<NotifyConfig, UpdateError> {
    let catalog = notify::config::WatchCatalog::load(store).map_err(store_error)?;
    let mut value = serde_json::to_value(current).map_err(|_| {
        UpdateError::Status(StatusCode::INTERNAL_SERVER_ERROR, "通知设置无法读取".into())
    })?;
    let mut patch = patch.clone();
    if let Some(qq) = patch.get_mut("qq").and_then(Value::as_object_mut) {
        qq.remove("appid_set");
        qq.remove("app_id_hint");
        if qq.get("app_id").is_some_and(Value::is_null) {
            qq.remove("app_id");
        }
    }
    let watch_saved = patch.get("watch").is_some();
    if let Some(watch) = patch.get_mut("watch").and_then(Value::as_object_mut) {
        watch.remove("migration_notice");
    }
    merge(&mut value, &patch);
    notify::config::migrate_watch(&mut value, &catalog, crate::storage::now())
        .map_err(UpdateError::Watch)?;
    let mut parsed: NotifyConfig = serde_json::from_value(value).map_err(|_| {
        if watch_saved {
            UpdateError::Watch(notify::config::WatchError::invalid())
        } else {
            UpdateError::Status(StatusCode::BAD_REQUEST, "通知设置格式无效".into())
        }
    })?;
    parsed.qq.app_id = parsed.qq.app_id.trim().to_string();
    for channel in &mut parsed.watch.channels {
        channel.normalize();
    }
    notify::config::validate_watch(&parsed.watch, &catalog, &current.watch)
        .map_err(UpdateError::Watch)?;
    validate(&parsed).map_err(|message| UpdateError::Status(StatusCode::BAD_REQUEST, message))?;
    if watch_saved {
        parsed.watch.migration_notice = None;
    }
    Ok(parsed)
}

/// Apply a settings update. Separate from the handler for tests; `writer`
/// is the single-writer check, consulted only when the body sets the openid.
///
/// Check order (contract): body validation (400), writer (409 `no_writer` /
/// `not_owner`), then revisions (409 `revision_conflict`). Config, AppSecret
/// and openid then land in ONE store transaction (review L7): all or none.
#[cfg(test)]
pub fn apply_update(
    store: &crate::storage::Store,
    update: &UpdateNotify,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
) -> Result<Value, UpdateError> {
    apply_update_with(store, update, node_id, writer, None, |_| Ok(()))
}

/// Sentinel that rolls a dry-run transaction back after every check.
#[derive(Debug)]
struct DryRun;
impl std::fmt::Display for DryRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dry run")
    }
}
impl std::error::Error for DryRun {}

/// Every check of `apply_update` (validation, writer, revisions) inside a
/// transaction that is then rolled back. Nothing is written.
pub fn check_update(
    store: &crate::storage::Store,
    update: &UpdateNotify,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
) -> Result<(), UpdateError> {
    let result = apply_update_with(store, update, node_id, writer, None, |_| {
        Err(std::io::Error::other(DryRun))
    });
    match result {
        Err(UpdateError::Status(_, message)) if message == "dry run" => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Ok(()),
    }
}

/// `apply_update` plus the Addendum F credential part, all in ONE store
/// transaction (rev 2 item 15). `after` runs last inside it (tests).
pub fn apply_update_with<G>(
    store: &crate::storage::Store,
    update: &UpdateNotify,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
    credentials: Option<store::CredentialsStep>,
    after: G,
) -> Result<Value, UpdateError>
where
    G: FnOnce(&mut crate::storage::Transaction<'_>) -> std::io::Result<()> + Send + 'static,
{
    apply_with_watch(store, update, node_id, writer, credentials, None, after)
}

/// Same transaction as `apply_update_with`, plus a follow-list revision bump
/// that patches only `watch` after the AppID write.
pub(crate) fn apply_with_watch<G>(
    store: &crate::storage::Store,
    update: &UpdateNotify,
    node_id: &str,
    writer: &dyn Fn() -> Result<(), Refusal>,
    credentials: Option<store::CredentialsStep>,
    watch: Option<store::WatchStep>,
    after: G,
) -> Result<Value, UpdateError>
where
    G: FnOnce(&mut crate::storage::Transaction<'_>) -> std::io::Result<()> + Send + 'static,
{
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
    // F rule 16: `qq.app_id: null` means unchanged and the read-only
    // `qq.appid_set` is ignored.
    let cleaned = update
        .config
        .as_ref()
        .filter(|patch| !patch.is_null())
        .map(|patch| {
            let mut patch = patch.clone();
            if let Some(qq) = patch.get_mut("qq").and_then(Value::as_object_mut) {
                qq.remove("appid_set");
                if qq.get("app_id").is_some_and(Value::is_null) {
                    qq.remove("app_id");
                }
            }
            patch
        });
    let patch = cleaned.as_ref();
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
        let parsed = updated_config(store, &current, patch)?;
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
    store::save_all_with(
        store,
        next.as_ref().map(|(cfg, expected)| (cfg, *expected)),
        secret_change,
        update.expected_secret_revision.unwrap_or(0),
        openid_write,
        credentials,
        watch,
        |_| Ok(()),
        after,
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

pub(crate) fn credentials_status(error: notify::credentials::SaveError) -> StatusCode {
    use notify::credentials::SaveError;
    match error {
        SaveError::Conflict { .. } | SaveError::HolderMoved | SaveError::WatchConflict { .. } => {
            StatusCode::CONFLICT
        }
        SaveError::Invalid | SaveError::WatchBaseRequired => StatusCode::BAD_REQUEST,
        SaveError::HolderUnreachable => StatusCode::SERVICE_UNAVAILABLE,
        SaveError::Store => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn credentials_refused(error: notify::credentials::SaveError) -> Response {
    use notify::credentials::SaveError;
    let status = credentials_status(error);
    let mut data = json!({ "code": error.code() });
    if let SaveError::Conflict { current } = error {
        data["current_generation"] = json!(current);
    }
    if let SaveError::WatchConflict { current } = error {
        data["current_revision"] = json!(current);
    }
    reply(status, Some(data), error.message())
}

/// Rule 1 (switch `REQUIRE_BASE_GENERATION`): 400 for a save or 清除
/// without a top-level `base_generation`.
pub(crate) fn base_generation_required() -> Response {
    reply(
        StatusCode::BAD_REQUEST,
        Some(json!({ "code": "base_generation_required" })),
        "Reload the settings and save again.",
    )
}

/// Addendum F: split the AppID/AppSecret part out of a settings update.
/// `None` when the update changes neither. The AppID counts as changed
/// only when it differs from the saved one.
pub fn credential_part(
    store: &crate::storage::Store,
    update: &UpdateNotify,
) -> std::io::Result<Option<notify::credentials::CredentialSave>> {
    let (current, _) = store::load_config(store)?;
    // F rule 16: absent or null = unchanged; an explicitly empty value is
    // still a 400 (the AppID cannot be cleared, rev 2 item 14).
    let appid = update
        .config
        .as_ref()
        .and_then(|patch| patch.pointer("/qq/app_id"))
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().unwrap_or(" ").trim().to_string())
        .filter(|appid| appid.is_empty() || *appid != current.qq.app_id.trim());
    let save = notify::credentials::CredentialSave {
        base_generation: update.base_generation,
        appid,
        appsecret: update
            .app_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        clear_appsecret: update.clear_app_secret,
        watch: None,
    };
    Ok(save.is_change().then_some(save))
}

/// Remove the parts `credential_part` took, so `apply_update` saves only
/// the rest of the settings. Returns whether anything else is left.
pub(crate) fn strip_credentials(update: &mut UpdateNotify) -> bool {
    if let Some(Value::Object(qq)) = update.config.as_mut().and_then(|patch| patch.get_mut("qq")) {
        qq.remove("app_id");
    }
    // Only the credential values: the openid in the same PUT is still
    // saved with the rest (wiping it here would drop it silently).
    if let Some(secret) = update.app_secret.as_mut() {
        zeroize::Zeroize::zeroize(secret);
    }
    update.app_secret = None;
    update.clear_app_secret = false;
    let config_rest = match update.config.as_ref() {
        None | Some(Value::Null) => false,
        Some(Value::Object(patch)) => patch
            .iter()
            .any(|(key, value)| key != "qq" || value.as_object().is_none_or(|qq| !qq.is_empty())),
        // Retain malformed config for validation; it cannot silently vanish
        // just because this PUT also includes valid credentials.
        Some(_) => true,
    };
    if !config_rest {
        // A credentials-only PUT (no config, `{}` or `{"qq":{}}`) saves no
        // settings, so it needs no `expected_revision`.
        update.config = None;
    }
    config_rest
        || update
            .admin_openid
            .as_deref()
            .is_some_and(|openid| !openid.trim().is_empty())
}

/// The follow list in this PUT, already migrated and validated against this
/// node's catalog. `None` when the body does not include `watch`.
pub(crate) fn watch_part(
    store: &crate::storage::Store,
    update: &UpdateNotify,
) -> Result<Option<notify::watch_sync::WatchSave>, UpdateError> {
    let Some(patch) = update
        .config
        .as_ref()
        .filter(|patch| patch.get("watch").is_some())
    else {
        return Ok(None);
    };
    let (current, _) = store::load_config(store).map_err(store_error)?;
    let desired = updated_config(store, &current, patch)?;
    Ok(Some(notify::watch_sync::WatchSave {
        base_revision: update.base_watch_revision,
        enabled: desired.watch.enabled,
        channels: desired.watch.channels,
        migration_notice: desired.watch.migration_notice,
    }))
}

/// Drop `watch` so a local preference write cannot replace the list the
/// holder just committed. Returns whether a watch object was removed.
pub(crate) fn strip_watch(update: &mut UpdateNotify) -> bool {
    let Some(config) = update.config.as_mut() else {
        return false;
    };
    let Some(object) = config.as_object_mut() else {
        return false;
    };
    let removed = object.remove("watch").is_some();
    if object.is_empty() {
        update.config = None;
    }
    removed
}

/// The gateway holder from this node's heartbeat view.
async fn current_holder(local: &str) -> notify::credentials::Holder {
    let Ok(cfg) = crate::config::load_config().await else {
        return notify::credentials::Holder::Nobody;
    };
    if !cfg.cluster.enabled {
        return notify::credentials::Holder::Nobody;
    }
    let digest = notify::authority::read_digest(&cfg, crate::storage::now());
    notify::credentials::pick_holder(local, notify::local_gateway_held(), &digest.nodes)
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
    // Addendum F: the AppID/AppSecret go to the gateway holder first. If
    // that fails nothing else is saved; the values are never echoed.
    // The follow list uses the same holder, then the same member copy.
    let credentials = match credential_part(&store, &update) {
        Ok(part) => part,
        Err(error) => {
            update.wipe();
            return reply(io_status(&error), None, error.to_string());
        }
    };
    let watch = match watch_part(&store, &update) {
        Ok(part) => part,
        Err(error) => {
            update.wipe();
            return error.into_response();
        }
    };
    if watch
        .as_ref()
        .is_some_and(|save| save.base_revision.is_none())
    {
        update.wipe();
        return credentials_refused(notify::credentials::SaveError::WatchBaseRequired);
    }
    if watch.is_some() {
        // The holder, or the local watch step, writes the list. Leaving it in
        // the preference patch would not bump the revision and could replace
        // a list just pulled back.
        strip_watch(&mut update);
    }
    let mut credentials_pushed = false;
    let mut local_credentials: Option<store::CredentialsStep> = None;
    let mut local_watch: Option<store::WatchStep> = None;
    let holder = current_holder(&node_id).await;
    let link = notify::openid_sync::SignedLink::new(Arc::clone(&store), node_id.clone());
    let forward_watch = match &holder {
        notify::credentials::Holder::Peer(peer) if link.watch_sync(peer) => watch.clone(),
        _ => None,
    };
    let watch_locally = if forward_watch.is_some() { None } else { watch };
    if credentials.is_some() || forward_watch.is_some() {
        if let Some(save) = credentials.as_ref() {
            if let Err(error) = notify::credentials::validate_save(save) {
                update.wipe();
                return credentials_refused(error);
            }
            if notify::credentials::base_generation_missing(
                save,
                notify::credentials::REQUIRE_BASE_GENERATION,
            ) {
                update.wipe();
                return base_generation_required();
            }
            strip_credentials(&mut update);
        }
        match &holder {
            notify::credentials::Holder::Local | notify::credentials::Holder::Nobody => {
                // Rev 2 item 15: committed with the rest in ONE transaction.
                if let Some(save) = credentials {
                    local_credentials = Some((save, node_id.clone(), crate::storage::now()));
                    credentials_pushed = true;
                }
                if let Some(watch) = watch_locally {
                    local_watch = Some((watch, node_id.clone(), crate::storage::now()));
                }
            }
            notify::credentials::Holder::Peer(peer) => {
                // Validate and pin local preferences before the holder
                // commits. Only credential-only revision bumps may rebase.
                let mut save = credentials.unwrap_or_default();
                save.watch = forward_watch;
                let check_store = Arc::clone(&store);
                let check_node = node_id.clone();
                let check_peer = peer.clone();
                let checked = tokio::task::spawn_blocking(move || {
                    let result =
                        forwarded::prepare(&check_store, &update, &check_node, &check_peer);
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
                let settings = match checked {
                    Ok(settings) => settings,
                    Err(error) => return error.into_response(),
                };
                let outcome = notify::credentials::save(
                    &store,
                    &link,
                    &notify::credentials::Holder::Peer(peer.clone()),
                    &save,
                    &node_id,
                    crate::storage::now(),
                )
                .await;
                drop(save);
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => return credentials_refused(error),
                };
                if outcome.changed {
                    tracing::info!(
                        "QQ 通知机器人凭据已由节点 {} 保存 (generation {})",
                        peer,
                        outcome.generation
                    );
                }
                if outcome.watch_changed {
                    tracing::info!(
                        "QQ 通知关注列表已由节点 {} 保存 (revision {})",
                        peer,
                        outcome.watch_revision.unwrap_or(0)
                    );
                }
                let completed =
                    forwarded::complete(Arc::clone(&store), &link, peer, outcome, settings, |_| {
                        Ok(())
                    })
                    .await;
                if completed.cache_ready {
                    let _ = notify::send_command(Command::Reload);
                } else {
                    tracing::warn!("QQ 通知已由节点 {} 保存，本节点同步待重试", peer);
                    notify::openid_sync::spawn_pull(Arc::clone(&store), node_id.clone());
                }
                // Holder has no watch_sync: commit here and push to members
                // that do. The credential commit on the holder already stands.
                if let Some(watch) = watch_locally {
                    let stored = Arc::clone(&store);
                    let node = node_id;
                    let saved = tokio::task::spawn_blocking(move || {
                        notify::watch_sync::apply_save(
                            &stored,
                            &watch,
                            &node,
                            crate::storage::now(),
                        )
                    })
                    .await
                    .unwrap_or(Err(notify::credentials::SaveError::Store));
                    match saved {
                        Ok(saved) if saved.changed => {
                            notify::openid_sync::spawn_push(push_store, push_node);
                        }
                        Ok(_) => {}
                        Err(error) => return credentials_refused(error),
                    }
                }
                return completed.into_response(cleared);
            }
        }
    } else if let Some(watch) = watch_locally {
        local_watch = Some((watch, node_id.clone(), crate::storage::now()));
    }
    let watch_before = local_watch
        .as_ref()
        .and(notify::watch_sync::load_meta(&store).ok());
    let result = tokio::task::spawn_blocking(move || {
        let result = apply_with_watch(
            &store,
            &update,
            &node_id,
            &|| notify::check_writer("修改管理员 openid"),
            local_credentials,
            local_watch,
            |_| Ok(()),
        );
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
            let watch_changed = watch_before.is_some_and(|before| {
                notify::watch_sync::load_meta(&push_store)
                    .is_ok_and(|meta| meta.revision != before.revision)
            });
            if openid_changed || credentials_pushed || watch_changed {
                // 1b / F: the new set goes to every current member.
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
    let credentials_sync = notify::openid_sync::peer_supports_credentials(&from);
    let watch_flag = headers
        .get(crate::cluster::peer_auth::NOTIFY_WATCH_SYNC_HEADER)
        .and_then(|value| value.to_str().ok());
    let watch_sync = notify::openid_sync::pull_sends_watch(
        notify::openid_sync::peer_supports_watch(&from),
        watch_flag,
    );
    let local = local_node_id().await;
    let result = tokio::task::spawn_blocking(move || {
        // A list saved before this build has revision 0. Seed it before the
        // copy is built so a joining node receives it without another save.
        if !local.is_empty() {
            notify::watch_sync::seed_if_needed(&store, &local, crate::storage::now())
                .map_err(|_| notify::openid_sync::ServeRefusal::Store)?;
        }
        notify::openid_sync::serve_pull(&store, &from, &members, credentials_sync, watch_sync)
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

/// `PUT /api/cluster/v1/notify/openid` (peer-only, signed, Addendum F): a
/// member forwards an AppID/AppSecret save to this node, the gateway
/// holder. Answers `{generation, changed, openid_cleared}` or a fixed
/// code; never a value.
pub(crate) async fn peer_credentials_save(
    peer: Option<Extension<crate::cluster::peer_call::AuthenticatedNode>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (from, store, members) = match peer_context(peer, &headers) {
        Ok(context) => context,
        Err(response) => return *response,
    };
    let local = local_node_id().await;
    let holds = matches!(
        current_holder(&local).await,
        notify::credentials::Holder::Local | notify::credentials::Holder::Nobody
    );
    let body = zeroize::Zeroizing::new(body.to_vec());
    let push_store = Arc::clone(&store);
    let push_node = local.clone();
    let result = tokio::task::spawn_blocking(move || {
        notify::credentials::serve_forward(
            &store,
            &from,
            &members,
            &body,
            holds,
            &local,
            crate::storage::now(),
        )
    })
    .await
    .unwrap_or(Err(notify::credentials::ForwardRefusal::Save(
        notify::credentials::SaveError::Store,
    )));
    match result {
        Ok(outcome) => {
            if outcome.changed || outcome.watch_changed {
                let _ = notify::send_command(Command::Reload);
                notify::openid_sync::spawn_push(push_store, push_node);
            }
            reply(StatusCode::OK, Some(json!(outcome)), "")
        }
        Err(notify::credentials::ForwardRefusal::NotMember) => {
            peer_refusal(notify::openid_sync::ServeRefusal::NotMember)
        }
        Err(notify::credentials::ForwardRefusal::Save(error)) => credentials_refused(error),
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

/// This node's ledger joined with fresh peer heartbeats.
fn joined_ledger(digest: &notify::observer::ClusterDigest, now: u64) -> notify::NotifyLedger {
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
    ledger
}

/// `nodes[].active_push` (Addendum E rev 2 item 10), logged-in only.
pub(crate) fn with_active_push(
    nodes: Vec<Value>,
    ledger: &notify::NotifyLedger,
    now: u64,
) -> Vec<Value> {
    nodes
        .into_iter()
        .map(|mut node| {
            let push = node
                .get("node_id")
                .and_then(Value::as_str)
                .and_then(|id| ledger.live.active_push.get(id))
                .map(|push| push.view(now))
                .unwrap_or_else(notify::live_events::ActivePush::empty_view);
            if let Some(object) = node.as_object_mut() {
                object.insert("active_push".into(), push);
            }
            node
        })
        .collect()
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
        let local_generation = notify::local_credentials_sync().0;
        let latest = notify::authority::latest_generation(&digest, local_generation);
        let local_authority = matches!(
            status.get("authority").and_then(Value::as_str),
            Some("primary" | "delegate")
        ) && local_generation >= latest;
        let fallback =
            status.get("authority").and_then(Value::as_str) == Some("no_quorum_fallback");
        let (nodes, writer_openid) = readiness_view(&status, &digest, local_authority);
        let authority_node = if local_authority || fallback {
            Some(digest.local.clone())
        } else {
            digest
                .nodes
                .iter()
                .find(|node| {
                    node.id != digest.local
                        && node.fresh
                        && node.notify_authority
                        && notify::authority::credentials_current(node, latest)
                })
                .map(|node| node.id.clone())
        };
        let sender = sender_view(authority_node.as_deref(), &digest, now);
        let ledger = joined_ledger(&digest, now);
        let nodes = with_active_push(nodes, &ledger, now);
        let sender_push = authority_node
            .as_deref()
            .and_then(|node| ledger.live.active_push.get(node))
            .map(|push| push.view(now))
            .unwrap_or_else(notify::live_events::ActivePush::empty_view);
        if let Some(object) = status.as_object_mut() {
            object.insert("authority_node_id".into(), json!(authority_node));
            object.insert("nodes".into(), json!(nodes));
            // Addendum E rev 2: the current sender's copy at the top level.
            object.insert("active_push".into(), sender_push);
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

    #[test]
    fn fresh_config_always_sends_live_events_enabled_true_and_the_budget() {
        let (_root, store) = temp_store();
        let mut view = settings_view(&store).unwrap();
        assert_eq!(view["config"]["live_events_enabled"], json!(true));
        // Worker stopped: the read-only fields are still present.
        live_events_view(&mut view, &json!({"worker": "stopped"}));
        assert_eq!(view["pending_live_events"], json!([]));
        assert_eq!(view["wakeup_remaining"], json!(0));
        assert_eq!(view["wakeup_reserve"], json!(2));
        assert_eq!(view["wakeup_next_close_at"], Value::Null);
        assert_eq!(view["passive_window_open"], json!(false));
        // Copied by name from a running worker's status, nothing else.
        let status = json!({
            "pending_live_events": ["live_end"],
            "quota": {"wakeup_remaining": 3, "wakeup_reserve": 2,
                      "wakeup_next_close_at": 1_800_086_400u64, "passive_window_open": true,
                      "admin_last_message_at": 1_800_000_000u64},
        });
        let mut view = settings_view(&store).unwrap();
        live_events_view(&mut view, &status);
        assert_eq!(view["pending_live_events"], json!(["live_end"]));
        assert_eq!(view["wakeup_remaining"], json!(3));
        assert_eq!(view["passive_window_open"], json!(true));
        assert!(view.get("admin_last_message_at").is_none());
        // Saved off, it reads back off; a stored record without the key is true.
        let patch = UpdateNotify {
            config: Some(json!({"live_events_enabled": false})),
            expected_revision: Some(view["revision"].as_u64().unwrap()),
            ..Default::default()
        };
        update(&store, patch).unwrap();
        assert_eq!(
            settings_view(&store).unwrap()["config"]["live_events_enabled"],
            json!(false)
        );
        let legacy: crate::notify::NotifyConfig = serde_json::from_str("{}").unwrap();
        assert!(legacy.live_events_enabled);
    }

    #[test]
    fn nodes_get_their_own_active_push_and_unknown_nodes_read_null() {
        let mut ledger = crate::notify::NotifyLedger::default();
        ledger.live.active_push.insert(
            "jp".into(),
            crate::notify::live_events::ActivePush {
                last_result: Some("rate_limited".into()),
                last_at: 1_800_000_000,
                refused_since: Some(1_800_000_000),
                month: crate::notify::live_events::shanghai_month(1_800_000_000),
                month_sent: 3,
            },
        );
        let nodes = with_active_push(
            vec![json!({"node_id": "jp"}), json!({"node_id": "ny"})],
            &ledger,
            1_800_000_100,
        );
        assert_eq!(nodes[0]["active_push"]["last_result"], "rate_limited");
        assert_eq!(nodes[0]["active_push"]["month_sent"], 3);
        assert_eq!(
            nodes[0]["active_push"]["refused_since"],
            "2027-01-15T16:00:00+08:00"
        );
        assert_eq!(nodes[1]["active_push"]["last_result"], Value::Null);
        assert_eq!(nodes[1]["active_push"]["month_sent"], 0);
    }

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
            other => panic!("{other:?}"),
        }
    }

    fn watch_catalog(store: &crate::storage::Store) {
        store
            .write(
                "channels.json",
                json!({"channels":[
                    {"name":"canonical","aliases":[],"niconico_name":"nico alias",
                     "platforms":{"youtube":"UCtest","twitch":"TestLogin","niconico":"nico-test"}}
                ]}),
            )
            .unwrap();
    }

    #[test]
    fn watch_reads_and_saves_group_legacy_entries_and_keep_capability_metadata() {
        let (_root, store) = temp_store();
        watch_catalog(&store);
        store
            .write(
                store::CONFIG_RECORD,
                json!({"watch":{"enabled":true,"channels":[
                    {"platform":"youtube","channel_id":"UCtest","name":"old"},
                    {"platform":"twitch","channel_id":"TESTLOGIN","name":"old"}
                ]}}),
            )
            .unwrap();
        let first = settings_view(&store).unwrap();
        let entries = json!([{"kind":"configured","name":"canonical"}]);
        assert_eq!(first["config"]["watch"]["channels"], entries);
        assert_eq!(
            first["configured_channels"],
            json!([{"name":"canonical","platforms":["youtube","twitch","niconico"]}])
        );
        assert_eq!(first["config"]["watch"]["migration_notice"]["folded"], 2);
        // Saving other preferences leaves the notice in place.
        let other = update(
            &store,
            UpdateNotify {
                config: Some(json!({"commands_enabled":false})),
                expected_revision: first["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            other["config"]["watch"]["migration_notice"],
            first["config"]["watch"]["migration_notice"]
        );
        // A stale legacy panel can still submit account entries. The saved
        // form is grouped, the notice is acknowledged, and GET/PUT agree.
        let saved = update(
            &store,
            UpdateNotify {
                config: Some(json!({"watch":{"enabled":true,"channels":[
                {"platform":"niconico","channel_id":"nico-test","name":"nico alias"}
            ],"migration_notice":{"at":0,"folded":999}}})),
                expected_revision: other["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(saved["config"]["watch"]["channels"], entries);
        assert!(saved["config"]["watch"]["migration_notice"].is_null());
        assert_eq!(saved["configured_channels"], first["configured_channels"]);
        assert_eq!(settings_view(&store).unwrap(), saved);
    }

    #[test]
    fn an_unresolved_configured_entry_can_be_saved_or_removed_but_not_newly_added() {
        let (_root, store) = temp_store();
        store
            .write(
                store::CONFIG_RECORD,
                json!({"watch":{"channels":[{"kind":"configured","name":"removed"}]}}),
            )
            .unwrap();
        let first = settings_view(&store).unwrap();
        assert_eq!(first["config"]["watch"]["channels"][0]["unresolved"], true);
        let saved = update(&store, UpdateNotify {
            config: Some(json!({"watch":{"enabled":true,"channels":first["config"]["watch"]["channels"]}})),
            expected_revision: first["revision"].as_u64(), ..Default::default()
        }).unwrap();
        assert_eq!(saved["config"]["watch"]["channels"][0]["name"], "removed");
        assert!(
            store.read(store::CONFIG_RECORD).unwrap().unwrap().value["watch"]["channels"][0]
                .get("unresolved")
                .is_none()
        );
        let removed = update(
            &store,
            UpdateNotify {
                config: Some(json!({"watch":{"channels":[]}})),
                expected_revision: saved["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        let error = update(
            &store,
            UpdateNotify {
                config: Some(
                    json!({"watch":{"channels":[{"kind":"configured","name":"removed"}]}}),
                ),
                expected_revision: removed["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            UpdateError::Watch(notify::config::WatchError {
                code: "watch_unknown_channel",
                ..
            })
        ));
    }

    #[test]
    fn historical_unmatched_watch_data_is_retained_unchanged_or_removed_only() {
        let (_root, store) = temp_store();
        watch_catalog(&store);
        store
            .write(
                store::CONFIG_RECORD,
                json!({"watch":{"enabled":true,"channels":[
                    {"platform":"youtube","channel_id":"UChistorical","name":"historical"}
                ]}}),
            )
            .unwrap();
        let first = settings_view(&store).unwrap();
        let retained = first["config"]["watch"]["channels"][0].clone();
        assert_eq!(
            retained,
            json!({"kind":"manual","name":"historical","platforms":{"youtube":"UChistorical"}})
        );
        // An unrelated save preserves the unavailable target and notice.
        let other = update(
            &store,
            UpdateNotify {
                config: Some(json!({"commands_enabled":false})),
                expected_revision: first["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(other["config"]["watch"], first["config"]["watch"]);
        let mut renamed = retained.clone();
        renamed["name"] = json!("edited");
        let mut retargeted = retained.clone();
        retargeted["platforms"]["youtube"] = json!("UCdifferent");
        for entry in [
            renamed,
            retargeted,
            // Even a catalog account cannot be newly submitted as manual.
            json!({"kind":"manual","name":"canonical","platforms":{"youtube":"UCtest"}}),
            json!({"kind":"manual","name":"forged","platforms":{"youtube":"UCexternal"},"legacy":true}),
            // A legacy flat body cannot bypass configured-only admission.
            json!({"platform":"youtube","channel_id":"UCexternal","name":"canonical"}),
        ] {
            let error = update(
                &store,
                UpdateNotify {
                    config: Some(json!({"watch":{"enabled":true,"channels":[entry]}})),
                    expected_revision: other["revision"].as_u64(),
                    app_secret: Some("synthetic-not-saved".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(matches!(
                error,
                UpdateError::Watch(notify::config::WatchError {
                    code: "invalid_field",
                    ..
                })
            ));
            assert_eq!(settings_view(&store).unwrap(), other);
            assert!(store::load_secret(&store).unwrap().0.is_none());
        }
        let saved = update(&store, UpdateNotify {
            config:Some(json!({"watch":{"channels":[retained,{"kind":"configured","name":"canonical"}]}})),
            expected_revision:other["revision"].as_u64(), ..Default::default()
        }).unwrap();
        assert_eq!(saved["config"]["watch"]["channels"][0], retained);
        assert!(saved["config"]["watch"]["migration_notice"].is_null());
        let removed = update(
            &store,
            UpdateNotify {
                config: Some(json!({"watch":{"channels":[]}})),
                expected_revision: saved["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(removed["config"]["watch"]["channels"], json!([]));
        let error = update(
            &store,
            UpdateNotify {
                config: Some(json!({"watch":{"channels":[retained]}})),
                expected_revision: removed["revision"].as_u64(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            UpdateError::Watch(notify::config::WatchError {
                code: "invalid_field",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn watch_refusals_use_stable_envelopes_and_do_not_write_any_settings() {
        let (_root, store) = temp_store();
        watch_catalog(&store);
        let too_many: Vec<_> = (0..51).map(|i| json!({"kind":"manual","name":format!("name{i}"),"platforms":{"youtube":format!("UC{i}")}})).collect();
        for (code, entries) in [
            (
                "watch_duplicate",
                json!([
                    {"kind":"configured","name":"canonical"},
                    {"kind":"configured","name":"canonical"}
                ]),
            ),
            (
                "watch_unknown_channel",
                json!([{"kind":"configured","name":"missing"}]),
            ),
            ("watch_too_many", json!(too_many)),
            (
                "invalid_field",
                json!([{"kind":"manual","name":"","platforms":{"youtube":"UCvalid"}}]),
            ),
            ("invalid_field", json!([{"kind":"unknown","name":"x"}])),
        ] {
            let error = update(
                &store,
                UpdateNotify {
                    config: Some(json!({"watch":{"enabled":true,"channels":entries}})),
                    app_secret: Some("synthetic-unsaved-secret".into()),
                    expected_revision: Some(0),
                    ..Default::default()
                },
            )
            .unwrap_err();
            let (status, response) = body(error.into_response()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["success"], false);
            assert_eq!(response["data"]["code"], code);
            assert_eq!(response["data"]["field"], "watch.channels");
            assert!(store.read(store::CONFIG_RECORD).unwrap().is_none());
            assert!(store::load_secret(&store).unwrap().0.is_none());
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
        // F rule 16: write-only; only the flag goes out.
        assert!(view["config"]["qq"].get("app_id").is_none());
        assert_eq!(view["config"]["qq"]["appid_set"], true);
        assert_eq!(view["config"]["qq"]["app_id_hint"], "7890");
        assert!(!view.to_string().contains("1234567890"));
        assert_eq!(
            crate::notify::store::load_config(&store)
                .unwrap()
                .0
                .qq
                .app_id,
            "1234567890"
        );
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
            other => panic!("{other:?}"),
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

    async fn body(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Every 503 from PUT /api/notify carries a stable JSON code: the
    /// holder-unreachable refusal (a save, an openid save with credentials,
    /// 清除) is `credentials_holder_unreachable`; any other 503 gets
    /// `notify_unavailable`, never a bare 503.
    #[tokio::test]
    async fn every_503_is_json_with_a_stable_code() {
        use crate::notify::credentials::SaveError;
        let (status, json) = body(credentials_refused(SaveError::HolderUnreachable)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["success"], false);
        assert_eq!(json["data"]["code"], "credentials_holder_unreachable");
        let (status, json) = body(reply(StatusCode::SERVICE_UNAVAILABLE, None, "x")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["data"]["code"], UNAVAILABLE_CODE);
        let (_, json) = body(reply(
            StatusCode::SERVICE_UNAVAILABLE,
            Some(json!({"code": "own"})),
            "x",
        ))
        .await;
        assert_eq!(json["data"]["code"], "own", "an existing code is kept");
        let (status, json) =
            body(UpdateError::Credentials(SaveError::HolderUnreachable).into_response()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["data"]["code"], "credentials_holder_unreachable");
        // Non-503 replies are unchanged.
        let (_, json) = body(reply(StatusCode::OK, None, "")).await;
        assert!(json["data"].is_null());
    }
}
