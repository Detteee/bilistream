use super::*;

// Capture frame from stream for crop selection
#[derive(Deserialize)]
pub struct CaptureFrameRequest {
    pub platform: String, // "youtube" or "twitch"
}

pub async fn capture_frame(
    axum::extract::Path(platform): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<ApiResponse<()>> {
    let cfg = match load_config().await {
        Ok(c) => c,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to load config: {}", e)),
            });
        }
    };

    // Get m3u8 URL based on platform
    let m3u8_url: String;
    let proxy: Option<String>;

    match platform.as_str() {
        "youtube" => {
            // Use channel_id from query params if provided, otherwise use config
            let channel_id = params
                .get("channel_id")
                .map(|s| s.as_str())
                .unwrap_or(&cfg.youtube.channel_id);

            // Use yt-dlp to get m3u8 URL with specific quality
            let channel_url = format!("https://www.youtube.com/channel/{}/live", channel_id);

            // Get yt-dlp command (handles Windows .exe)
            let yt_dlp_cmd = if cfg!(target_os = "windows") {
                if let Ok(exe_path) = std::env::current_exe() {
                    if let Some(exe_dir) = exe_path.parent() {
                        let local_yt_dlp = exe_dir.join("yt-dlp.exe");
                        if local_yt_dlp.exists() {
                            local_yt_dlp.to_string_lossy().to_string()
                        } else {
                            "yt-dlp".to_string()
                        }
                    } else {
                        "yt-dlp".to_string()
                    }
                } else {
                    "yt-dlp".to_string()
                }
            } else {
                "yt-dlp".to_string()
            };

            let mut cmd = tokio::process::Command::new(yt_dlp_cmd);
            // Use -f to specify quality format, then -g to get URL
            cmd.arg("-f")
                .arg(&cfg.youtube.quality)
                .arg("-g")
                .arg(&channel_url);
            crate::plugins::utils::add_yt_dlp_cookies_args(
                cmd.as_std_mut(),
                &cfg.youtube.cookies_file,
                &cfg.youtube.cookies_from_browser,
            );

            if let Some(ref proxy_url) = cfg.youtube.proxy {
                cmd.arg("--proxy").arg(proxy_url);
            }

            match cmd.output().await {
                Ok(output) if output.status.success() => {
                    m3u8_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if m3u8_url.is_empty() {
                        return Json(ApiResponse {
                            success: false,
                            data: None,
                            message: Some(
                                "YouTube stream is not live or URL not found".to_string(),
                            ),
                        });
                    }
                    proxy = cfg.youtube.proxy.clone();
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::error!("yt-dlp failed: {}", stderr);

                    // Check if it's a "not live" error
                    let message = if stderr.contains("will begin in")
                        || stderr.contains("not live")
                        || stderr.contains("Premieres in")
                    {
                        "该频道未在直播".to_string()
                    } else {
                        format!(
                            "获取直播流失败: {}",
                            stderr.lines().next().unwrap_or("未知错误")
                        )
                    };

                    return Json(ApiResponse {
                        success: false,
                        data: None,
                        message: Some(message),
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to execute yt-dlp: {}", e);
                    return Json(ApiResponse {
                        success: false,
                        data: None,
                        message: Some(format!(
                            "Failed to execute yt-dlp: {}. Make sure yt-dlp is installed and in PATH.",
                            e
                        )),
                    });
                }
            }
        }
        "twitch" => {
            // Use streamlink with proxy URL like in twitch.rs
            let proxy_region = &cfg.twitch.proxy_region;
            let proxy_url = match proxy_region.as_str() {
                "na" => "--twitch-proxy-playlist=https://lb-na.cdn-perfprod.com",
                "eu" => "--twitch-proxy-playlist=https://lb-eu.cdn-perfprod.com",
                "eu2" => "--twitch-proxy-playlist=https://lb-eu2.cdn-perfprod.com",
                "eu3" => "--twitch-proxy-playlist=https://lb-eu3.cdn-perfprod.com",
                "eu4" => "--twitch-proxy-playlist=https://lb-eu4.cdn-perfprod.com",
                "eu5" => "--twitch-proxy-playlist=https://lb-eu5.cdn-perfprod.com",
                "as" => "--twitch-proxy-playlist=https://lb-as.cdn-perfprod.com",
                "sa" => "--twitch-proxy-playlist=https://lb-sa.cdn-perfprod.com",
                "eul" => "--twitch-proxy-playlist=https://eu.luminous.dev",
                "eu2l" => "--twitch-proxy-playlist=https://eu2.luminous.dev",
                "asl" => "--twitch-proxy-playlist=https://as.luminous.dev",
                "" => "",
                _ => "asl", // Default to asl if invalid
            };

            let channel_url = format!("https://twitch.tv/{}", cfg.twitch.channel_id);

            let mut cmd = tokio::process::Command::new("streamlink");

            // Add proxy URL if not empty
            if !proxy_url.is_empty() {
                cmd.arg(proxy_url);
            }

            // Use configured quality instead of "best"
            cmd.arg("--stream-url")
                .arg(&channel_url)
                .arg(&cfg.twitch.quality);

            if let Some(ref http_proxy) = cfg.twitch.proxy {
                cmd.arg("--http-proxy").arg(http_proxy);
            }

            match cmd.output().await {
                Ok(output) if output.status.success() => {
                    m3u8_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if m3u8_url.is_empty() {
                        return Json(ApiResponse {
                            success: false,
                            data: None,
                            message: Some("Twitch stream is not live or URL not found".to_string()),
                        });
                    }
                    proxy = cfg.twitch.proxy.clone();
                }
                _ => {
                    return Json(ApiResponse {
                        success: false,
                        data: None,
                        message: Some(
                            "Failed to get Twitch stream URL. Make sure streamlink and streamlink-ttvlol plugin are installed."
                                .to_string(),
                        ),
                    });
                }
            }
        }
        _ => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some("Invalid platform".to_string()),
            });
        }
    }

    // Capture frame using ffmpeg
    let output_path = match std::env::current_exe() {
        Ok(path) => path.with_file_name("pic_for_crop.jpg"),
        Err(_) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some("Failed to get executable path".to_string()),
            });
        }
    };

    // Get ffmpeg command (handles Windows .exe)
    let ffmpeg_cmd = if cfg!(target_os = "windows") {
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_ffmpeg = exe_dir.join("ffmpeg.exe");
                if local_ffmpeg.exists() {
                    local_ffmpeg.to_string_lossy().to_string()
                } else {
                    "ffmpeg".to_string()
                }
            } else {
                "ffmpeg".to_string()
            }
        } else {
            "ffmpeg".to_string()
        }
    } else {
        "ffmpeg".to_string()
    };

    let mut cmd = tokio::process::Command::new(ffmpeg_cmd);
    cmd.arg("-y")
        .arg("-live_start_index")
        .arg("-1")
        .arg("-i")
        .arg(&m3u8_url)
        .arg("-vframes")
        .arg("1")
        .arg(&output_path);

    if let Some(proxy_url) = proxy {
        cmd.arg("-http_proxy").arg(proxy_url);
    }

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to execute ffmpeg: {}", e)),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("FFmpeg failed: {}", stderr)),
        });
    }

    // Read the image and convert to base64
    let image_data = match tokio::fs::read(&output_path).await {
        Ok(data) => data,
        Err(e) => {
            return Json(ApiResponse {
                success: false,
                data: None,
                message: Some(format!("Failed to read captured image: {}", e)),
            });
        }
    };

    let base64_image =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image_data);

    // Return base64 image in message field since data field must be ()
    Json(ApiResponse {
        success: true,
        data: Some(()),
        message: Some(format!("data:image/jpeg;base64,{}", base64_image)),
    })
}

// Update crop configuration
#[derive(Deserialize)]
pub struct UpdateCropRequest {
    pub platform: String, // "youtube" or "twitch"
    pub enabled: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
}

pub async fn update_crop(
    Json(payload): Json<UpdateCropRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let crop_config = match crop_config_from_update(&payload) {
        Ok(crop_config) => crop_config,
        Err(message) => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some(message),
            });
        }
    };

    match payload.platform.as_str() {
        "youtube" => {
            cfg.youtube.crop = crop_config;
        }
        "twitch" => {
            cfg.twitch.crop = crop_config;
        }
        _ => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some("Invalid platform".to_string()),
            });
        }
    }

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("Crop configuration updated".to_string()),
    })
}

pub(crate) fn crop_config_from_update(
    payload: &UpdateCropRequest,
) -> Result<Option<crate::config::CropConfig>, String> {
    if !payload.enabled {
        return Ok(None);
    }

    let (Some(width), Some(height), Some(x), Some(y)) =
        (payload.width, payload.height, payload.x, payload.y)
    else {
        return Err("Crop dimensions required when enabled".to_string());
    };

    if width == 0 || height == 0 {
        return Err("Crop width and height must be greater than 0".to_string());
    }

    Ok(Some(crate::config::CropConfig {
        width,
        height,
        x,
        y,
    }))
}

// Get current crop configuration
pub async fn get_crop(
    platform: String,
) -> Result<Json<ApiResponse<Option<crate::config::CropConfig>>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let crop = match platform.as_str() {
        "youtube" => cfg.youtube.crop,
        "twitch" => cfg.twitch.crop,
        _ => {
            return Ok(Json(ApiResponse {
                success: false,
                data: None,
                message: Some("Invalid platform".to_string()),
            }));
        }
    };

    Ok(Json(ApiResponse {
        success: true,
        data: Some(crop),
        message: None,
    }))
}

#[derive(Deserialize)]
pub struct UpdateFfmpegCacheRequest {
    pub platform: String,
    pub enabled: bool,
    pub latency_secs: Option<u64>,
}

pub async fn update_ffmpeg_cache(
    Json(payload): Json<UpdateFfmpegCacheRequest>,
) -> Result<ApiResponse<()>, StatusCode> {
    let mut cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let cache = match payload.platform.as_str() {
        "youtube" => &mut cfg.youtube.ffmpeg_cache,
        "twitch" => &mut cfg.twitch.ffmpeg_cache,
        _ => {
            return Ok(ApiResponse {
                success: false,
                data: None,
                message: Some("Invalid platform".to_string()),
            });
        }
    };

    cache.enabled = payload.enabled;
    if let Some(latency_secs) = payload.latency_secs {
        cache.latency_secs = latency_secs.clamp(1, 60);
    }

    crate::config::save_config(&cfg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    set_config_updated();
    refresh_status_cache_config_from(&cfg);

    Ok(ApiResponse {
        success: true,
        data: None,
        message: Some("HLS cache configuration updated".to_string()),
    })
}

pub async fn get_ffmpeg_cache(
    platform: String,
) -> Result<Json<ApiResponse<crate::config::FfmpegCache>>, StatusCode> {
    let cfg = load_config()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let cache = match platform.as_str() {
        "youtube" => cfg.youtube.ffmpeg_cache.clone(),
        "twitch" => cfg.twitch.ffmpeg_cache.clone(),
        _ => {
            return Ok(Json(ApiResponse {
                success: false,
                data: None,
                message: Some("Invalid platform".to_string()),
            }));
        }
    };

    Ok(Json(ApiResponse {
        success: true,
        data: Some(cache),
        message: None,
    }))
}
