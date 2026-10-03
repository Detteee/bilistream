//! YouTube API usage and discovery status for settings.
use crate::config::load_config;
use axum::Json;
use serde_json::json;

/// YouTube Data API key pool and uploads-playlist polling for the settings
/// view. Keys appear only as fingerprints.
pub async fn get_status() -> Json<serde_json::Value> {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };
    let keys = cfg.youtube_api_keys();
    if keys.is_empty() {
        return Json(json!({ "success": true, "data": { "configured": false } }));
    }
    let mut data = crate::plugins::youtube_data::key_pool_status(&keys);
    data["configured"] = json!(true);
    data["playlist"] = json!(crate::plugins::youtube_discovery::playlist_status());
    data["websub"] = json!(crate::plugins::youtube_websub::status());
    Json(json!({ "success": true, "data": data }))
}
