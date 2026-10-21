//! Holodex account-status HTTP route.
use crate::config::load_config;
use crate::plugins::holodex_account::{apply_holodex_jwt_sync, ensure_holodex_username};
use axum::Json;
use serde_json::json;

pub async fn auth_status() -> Json<serde_json::Value> {
    let mut cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({
                "success": false,
                "message": format!("Failed to load config: {}", e)
            }));
        }
    };

    if cfg.holodex_api_key.as_ref().is_none_or(|k| k.is_empty()) {
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": false,
                "message": "Holodex API key not configured"
            }
        }));
    }

    let jwt = match cfg.holodex_jwt.as_ref().filter(|j| !j.is_empty()) {
        Some(jwt) => jwt.clone(),
        None => {
            return Json(json!({
                "success": true,
                "data": {
                    "logged_in": false,
                    "skip_jwt_verify": cfg.holodex_skip_jwt_verify
                }
            }));
        }
    };

    if cfg.holodex_skip_jwt_verify {
        ensure_holodex_username(&mut cfg, &jwt).await;
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": true,
                "username": cfg.holodex_username,
                "skip_jwt_verify": true,
                "jwt_refreshed": false
            }
        }));
    }

    let (active_jwt, jwt_refreshed) = match apply_holodex_jwt_sync(&mut cfg).await {
        Ok(result) => result,
        Err(e) => {
            if crate::plugins::holodex::holodex_jwt_is_expired(&jwt) {
                return Json(json!({
                    "success": true,
                    "data": {
                        "logged_in": false,
                        "expired": true,
                        "message": e
                    }
                }));
            }
            tracing::warn!(
                "Holodex 登录状态同步失败，已跳过：{}",
                crate::logging::safe_diagnostic(&e.to_string())
            );
            (jwt, false)
        }
    };

    if crate::plugins::holodex::holodex_jwt_is_expired(&active_jwt) {
        return Json(json!({
            "success": true,
            "data": {
                "logged_in": false,
                "expired": true
            }
        }));
    }

    Json(json!({
        "success": true,
        "data": {
            "logged_in": true,
            "username": cfg.holodex_username,
            "skip_jwt_verify": cfg.holodex_skip_jwt_verify,
            "jwt_refreshed": jwt_refreshed
        }
    }))
}
