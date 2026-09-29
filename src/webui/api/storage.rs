use super::*;
use base64::Engine;
use std::io;

fn storage_response<T: Serialize>(result: io::Result<T>, message: &str) -> Response {
    match result {
        Ok(data) => (
            StatusCode::OK,
            Json(ApiResponse {
                success: true,
                data: Some(data),
                message: Some(message.to_owned()),
            }),
        )
            .into_response(),
        Err(error) => {
            let status = match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(ApiResponse::<()> {
                    success: false,
                    data: None,
                    message: Some(error.to_string()),
                }),
            )
                .into_response()
        }
    }
}

pub async fn storage_status() -> Json<serde_json::Value> {
    let result = tokio::task::spawn_blocking(crate::storage::global).await;
    match result {
        Ok(Ok(store)) => Json(
            json!({"ready":true,"schema":1,"sqlite_version":rusqlite::version(),"configured":store.revision("config.json").ok().flatten().is_some()}),
        ),
        Ok(Err(error)) => Json(json!({"ready":false,"message":error.to_string()})),
        Err(_) => Json(json!({"ready":false,"message":"数据存储暂不可用"})),
    }
}

pub async fn youtube_cookie_status() -> Response {
    storage_response(crate::plugins::youtube_cookies::status(), "")
}

#[derive(Deserialize)]
pub struct CookieImport {
    content: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
pub struct Revision {
    expected_revision: u64,
}

pub async fn import_youtube_cookies(Json(payload): Json<CookieImport>) -> Response {
    storage_response(
        crate::plugins::youtube_cookies::import(payload.content, payload.expected_revision).await,
        "Cookie 已加密保存",
    )
}

pub async fn clear_youtube_cookies(Json(payload): Json<Revision>) -> Response {
    storage_response(
        crate::plugins::youtube_cookies::clear(payload.expected_revision).await,
        "Cookie 已清除",
    )
}

pub async fn get_player_filter() -> Response {
    let result = (|| {
        let doc = crate::storage::global()?
            .read("invalid_words.txt")?
            .ok_or_else(|| io::Error::other("过滤词不可用"))?;
        Ok(json!({"content":doc.value,"revision":doc.revision}))
    })();
    storage_response(result, "")
}

pub async fn save_player_filter(Json(payload): Json<CookieImport>) -> Response {
    if payload.content.len() > 1024 * 1024 || payload.content.contains('\0') {
        return storage_response::<()>(
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "过滤词内容无效或过大",
            )),
            "",
        );
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::storage::global()?.compare_exchange(
            "invalid_words.txt",
            payload.expected_revision,
            serde_json::Value::String(payload.content),
        )
    })
    .await
    .map_err(io::Error::other)
    .and_then(|r| r);
    storage_response(result, "过滤词已保存")
}

#[derive(Deserialize)]
pub struct BackupRequest {
    password: String,
}

pub async fn export_storage_backup(Json(payload): Json<BackupRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        crate::storage::global()?.export_backup(&payload.password)
    })
    .await
    .map_err(io::Error::other)
    .and_then(|r| r);
    match result {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=bilistream.backup",
                ),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => storage_response::<()>(Err(error), ""),
    }
}

#[derive(Deserialize)]
pub struct RestoreRequest {
    password: String,
    backup_base64: String,
}

pub async fn restore_storage_backup(Json(payload): Json<RestoreRequest>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        if payload.backup_base64.len() > 48 * 1024 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "备份文件过大"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.backup_base64)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "备份格式无效"))?;
        crate::storage::global()?.restore_backup(&payload.password, &bytes)?;
        crate::webui::events::publish(crate::webui::events::CONFIG);
        crate::plugins::set_config_updated();
        crate::webui::state::request_status_refresh();
        Ok(())
    })
    .await
    .map_err(io::Error::other)
    .and_then(|r| r);
    storage_response(result, "备份已恢复")
}

/// A small capability endpoint lets peers avoid sending secret-stripped
/// full config to old binaries that overwrite node-local source settings.
pub async fn storage_sync_capabilities() -> Json<serde_json::Value> {
    Json(json!({"config_sync":2,"node_local_credentials":true}))
}
