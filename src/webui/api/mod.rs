use axum::{
    extract::{Json, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::state::{
    get_logs, get_status_cache, refresh_status_cache_config_from, update_status_cache,
    update_status_cache_with, BiliStatus, NetworkStatus, TwStatus, YtStatus,
};
use crate::config::{load_config, Config};
use crate::plugins::{
    bili_change_live_title, bili_start_live, bili_stop_live, bili_update_area, bilibili,
    get_bili_live_status, get_ffmpeg_cache_speed, get_ffmpeg_network_stats, get_ffmpeg_speed,
    is_ffmpeg_hls_cache_active, send_danmaku as send_danmaku_to_bili, set_config_updated,
};
use crate::updater;

mod config;
mod crop;
mod holodex;
mod manage;
mod setup;
mod status;
mod stream;

pub use config::*;
pub use crop::*;
pub use holodex::*;
pub use manage::*;
pub use setup::*;
pub use status::*;
pub use stream::*;

fn config_save_status(error: Box<dyn std::error::Error>) -> StatusCode {
    if error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    {
        StatusCode::CONFLICT
    } else {
        tracing::error!("Configuration save failed: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[derive(Serialize)]
pub struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    message: Option<String>,
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_edits_merge_unrelated_changes_but_reject_stale_fields() {
        let expected = HashMap::from([("interval".to_string(), json!(30))]);
        let patch = json!({"interval": 75, "auto_cover": null});
        assert!(validate_edit_preconditions(
            &json!({"interval": 30, "auto_cover": true}),
            &patch,
            Some(&expected)
        )
        .is_ok());
        assert_eq!(
            validate_edit_preconditions(
                &json!({"interval": 45, "auto_cover": true}),
                &patch,
                Some(&expected)
            ),
            Err(EDIT_CONFLICT)
        );
        assert_eq!(
            validate_edit_preconditions(
                &json!({"interval": 30, "auto_cover": true}),
                &json!({"interval": 75, "auto_cover": false}),
                Some(&expected)
            ),
            Err(EDIT_INVALID)
        );
        assert!(validate_edit_preconditions(&json!({}), &patch, None).is_ok());
    }

    #[test]
    fn keyword_preconditions_compare_array_contents() {
        let expected = HashMap::from([("streaming_banned_keywords".to_string(), json!(["old"]))]);
        let patch = json!({"streaming_banned_keywords": []});
        assert_eq!(
            validate_edit_preconditions(
                &json!({"streaming_banned_keywords": ["new"]}),
                &patch,
                Some(&expected)
            ),
            Err(EDIT_CONFLICT)
        );
        assert!(validate_edit_preconditions(
            &json!({"streaming_banned_keywords": ["old"]}),
            &patch,
            Some(&expected)
        )
        .is_ok());
    }

    #[test]
    fn qr_images_are_local_svg_and_oversized_payloads_fail() {
        use base64::Engine as _;
        let image = qr_code_data_url("https://example.invalid/login?token=测试").unwrap();
        let encoded = image.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        assert!(svg.contains("<svg"));
        assert!(!svg.contains("example.invalid"));
        assert!(qr_code_data_url(&"x".repeat(10000)).is_err());
    }

    #[test]
    fn monitor_target_reload_needed_only_for_enable_or_enabled_channel_change() {
        assert!(!monitor_target_reload_needed(
            true, true, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, false, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            false, true, "Channel", "Channel", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, true, "Channel", "Other", "id", "id",
        ));
        assert!(monitor_target_reload_needed(
            true, true, "Channel", "Channel", "id", "other-id",
        ));
        assert!(!monitor_target_reload_needed(
            false, false, "Channel", "Other", "id", "other-id",
        ));
    }

    #[test]
    fn crop_update_validation_rejects_incomplete_or_zero_size() {
        let disabled = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: false,
            width: None,
            height: None,
            x: None,
            y: None,
        };
        assert!(crop_config_from_update(&disabled).unwrap().is_none());

        let missing = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(1920),
            height: None,
            x: Some(0),
            y: Some(0),
        };
        assert_eq!(
            crop_config_from_update(&missing).unwrap_err(),
            "Crop dimensions required when enabled"
        );

        let zero_width = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(0),
            height: Some(1080),
            x: Some(0),
            y: Some(0),
        };
        assert_eq!(
            crop_config_from_update(&zero_width).unwrap_err(),
            "Crop width and height must be greater than 0"
        );

        let valid = UpdateCropRequest {
            platform: "youtube".to_string(),
            enabled: true,
            width: Some(1280),
            height: Some(720),
            x: Some(10),
            y: Some(20),
        };
        let crop = crop_config_from_update(&valid)
            .unwrap()
            .expect("valid crop should be returned");
        assert_eq!(crop.width, 1280);
        assert_eq!(crop.height, 720);
        assert_eq!(crop.x, 10);
        assert_eq!(crop.y, 20);
    }

    #[test]
    fn ordered_channel_ids_deduplicates_without_losing_order() {
        let mut channel_ids = OrderedChannelIds::default();

        assert!(channel_ids.insert(" first "));
        assert!(channel_ids.insert("second"));
        assert!(!channel_ids.insert("first"));
        assert!(!channel_ids.insert("  "));

        let (ordered, seen) = channel_ids.into_parts();
        assert_eq!(ordered, vec!["first".to_string(), "second".to_string()]);
        assert!(seen.contains("first"));
        assert!(seen.contains("second"));
        assert_eq!(seen.len(), 2);
    }
}
