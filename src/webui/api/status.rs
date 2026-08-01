use super::*;

pub(crate) static STATUS_REFRESH_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

pub async fn refresh_status_cache_config() {
    if let Ok(cfg) = load_config().await {
        refresh_status_cache_config_from(&cfg);
    }
}

// Refresh live status in background (like refresh buttons)
pub async fn refresh_live_status_background() {
    // Spawn background tasks to refresh live status without blocking
    tokio::spawn(async {
        let _ = refresh_youtube_status().await;
    });

    tokio::spawn(async {
        let _ = refresh_twitch_status().await;
    });
}

pub fn start_status_refresh_worker() {
    if STATUS_REFRESH_WORKER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    tokio::spawn(async {
        loop {
            let interval_secs = match refresh_status_snapshot().await {
                Ok(interval_secs) => interval_secs.max(5),
                Err(e) => {
                    tracing::debug!("WebUI status refresh skipped: {}", e);
                    15
                }
            };

            // Sleep until the poll interval elapses or a state change asks for
            // an immediate refresh.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
                _ = crate::webui::state::status_refresh_requested() => {}
            }
        }
    });
}

pub(crate) async fn refresh_status_snapshot() -> Result<u64, String> {
    let cfg = load_config().await.map_err(|e| e.to_string())?;
    refresh_status_cache_config_from(&cfg);

    if let Err(e) = refresh_bilibili_status_cache_with_config(&cfg).await {
        tracing::warn!("WebUI Bilibili status refresh failed: {}", e);
    }

    if cfg.youtube.enable_monitor && !cfg.youtube.channel_id.is_empty() {
        if let Err(e) = refresh_youtube_status_cache_with_config(&cfg).await {
            tracing::warn!("WebUI YouTube status refresh failed: {}", e);
        }
    } else {
        update_status_cache_with(|status| status.youtube = None);
    }

    if cfg.twitch.enable_monitor && !cfg.twitch.channel_id.is_empty() {
        if let Err(e) = refresh_twitch_status_cache_with_config(&cfg).await {
            tracing::warn!("WebUI Twitch status refresh failed: {}", e);
        }
    } else {
        update_status_cache_with(|status| status.twitch = None);
    }

    Ok(cfg.interval)
}

pub(crate) async fn refresh_bilibili_status_cache_with_config(cfg: &Config) -> Result<(), String> {
    let (is_live, title, area_id) = get_bili_live_status(cfg.bililive.room)
        .await
        .map_err(|e| e.to_string())?;
    let area_name = crate::plugins::get_area_name(area_id)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", area_id));

    update_status_cache_with(|status| {
        status.bilibili.is_live = is_live;
        status.bilibili.title = title;
        status.bilibili.area_id = area_id;
        status.bilibili.area_name = area_name;
        status.bilibili.enable_danmaku_command = cfg.bililive.enable_danmaku_command;
    });

    Ok(())
}

pub(crate) async fn refresh_youtube_status_cache_with_config(cfg: &Config) -> Result<(), String> {
    if cfg.youtube.channel_id.is_empty() {
        return Err("YouTube channel not configured".to_string());
    }

    let streams =
        crate::plugins::holodex::get_holodex_streams(vec![cfg.youtube.channel_id.clone()], false)
            .await
            .map_err(|e| e.to_string())?;

    let channel_streams: Vec<_> = streams
        .iter()
        .filter(|s| s.channel.id == cfg.youtube.channel_id)
        .collect();

    let (is_live, topic, title) = if let Some(live_stream) =
        channel_streams.iter().find(|s| s.status == "live")
    {
        (
            true,
            live_stream.topic_id.clone(),
            Some(live_stream.title.clone()),
        )
    } else if let Some(upcoming_stream) = channel_streams.iter().find(|s| s.status == "upcoming") {
        (
            false,
            upcoming_stream.topic_id.clone(),
            Some(upcoming_stream.title.clone()),
        )
    } else {
        (false, None, None)
    };

    let area_name = crate::plugins::get_area_name(cfg.youtube.area_v2)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.youtube.area_v2));

    update_status_cache_with(|status| {
        status.youtube = Some(YtStatus {
            is_live,
            title,
            topic,
            channel_name: cfg.youtube.channel_name.clone(),
            channel_id: cfg.youtube.channel_id.clone(),
            quality: cfg.youtube.quality.clone(),
            area_id: cfg.youtube.area_v2,
            area_name,
            crop_enabled: cfg.youtube.crop.is_some(),
            ffmpeg_cache_enabled: cfg.youtube.ffmpeg_cache.enabled,
            ffmpeg_cache_latency_secs: cfg.youtube.ffmpeg_cache.latency_secs,
        });
    });

    Ok(())
}

pub(crate) async fn refresh_twitch_status_cache_with_config(cfg: &Config) -> Result<(), String> {
    if cfg.twitch.channel_id.is_empty() {
        return Err("Twitch channel not configured".to_string());
    }

    let (is_live, game, title, _) = crate::plugins::get_twitch_status(&cfg.twitch.channel_id)
        .await
        .map_err(|e| e.to_string())?;
    let area_name = crate::plugins::get_area_name(cfg.twitch.area_v2)
        .unwrap_or_else(|| format!("未知分区 (ID: {})", cfg.twitch.area_v2));

    update_status_cache_with(|status| {
        status.twitch = Some(TwStatus {
            is_live,
            title,
            game,
            channel_name: cfg.twitch.channel_name.clone(),
            channel_id: cfg.twitch.channel_id.clone(),
            quality: cfg.twitch.quality.clone(),
            area_id: cfg.twitch.area_v2,
            area_name,
            crop_enabled: cfg.twitch.crop.is_some(),
            ffmpeg_cache_enabled: cfg.twitch.ffmpeg_cache.enabled,
            ffmpeg_cache_latency_secs: cfg.twitch.ffmpeg_cache.latency_secs,
        });
    });

    Ok(())
}

pub(crate) async fn apply_realtime_stream_metrics(bili: &mut BiliStatus) {
    let hls_cache_active = is_ffmpeg_hls_cache_active();
    let stream_speed = get_ffmpeg_speed();
    let stream_cache_speed = if hls_cache_active {
        get_ffmpeg_cache_speed()
    } else {
        None
    };
    let network_stats = get_ffmpeg_network_stats();

    bili.stream_quality = if bili.is_live {
        stream_speed.map(|speed| {
            if speed > 0.97 {
                "流畅".to_string()
            } else if speed > 0.94 {
                "波动".to_string()
            } else {
                "卡顿".to_string()
            }
        })
    } else {
        None
    };
    bili.stream_speed = stream_speed;
    bili.stream_cache_speed = stream_cache_speed;
    bili.stream_bitrate_kbps = network_stats.push_bitrate_kbps;
    bili.stream_cache_bitrate_kbps = if hls_cache_active {
        network_stats.cache_bitrate_kbps
    } else {
        None
    };
    bili.stream_fps = network_stats.push_fps;
    bili.stream_frame = network_stats.push_frame;
    bili.stream_total_bytes = network_stats.push_total_bytes;
    bili.stream_cache_total_bytes = if hls_cache_active {
        network_stats.cache_total_bytes
    } else {
        0
    };
    bili.hls_cache_active = hls_cache_active;
}

pub async fn get_status() -> impl IntoResponse {
    if get_status_cache().is_none() {
        if let Err(e) = load_config().await {
            // Only log error if it's not a "file not found" error (expected on first run)
            let is_not_found = e.to_string().contains("No such file");
            if !is_not_found {
                tracing::error!("Failed to load config: {}", e);
            }

            let error_msg = if e.to_string().contains("Permission denied") {
                format!("配置文件权限错误: {}。请确保 config.json 文件存在且有读取权限，或在可执行文件所在目录运行程序。", e)
            } else if is_not_found {
                "配置文件不存在，请完成首次设置".to_string()
            } else {
                format!("配置加载失败: {}", e)
            };
            return (
                StatusCode::OK,
                Json(ApiResponse::<()> {
                    success: false,
                    data: None,
                    message: Some(error_msg),
                }),
            )
                .into_response();
        }

        refresh_status_cache_config().await;
    }

    let mut status = get_status_cache().unwrap_or_default();
    apply_realtime_stream_metrics(&mut status.bilibili).await;

    (
        StatusCode::OK,
        Json(ApiResponse {
            success: true,
            data: Some(status),
            message: None,
        }),
    )
        .into_response()
}

pub async fn get_network_status() -> Json<ApiResponse<NetworkStatus>> {
    let hls_cache_active = is_ffmpeg_hls_cache_active();
    let network_stats = get_ffmpeg_network_stats();

    Json(ApiResponse {
        success: true,
        data: Some(NetworkStatus {
            stream_speed: get_ffmpeg_speed(),
            stream_cache_speed: if hls_cache_active {
                get_ffmpeg_cache_speed()
            } else {
                None
            },
            stream_bitrate_kbps: network_stats.push_bitrate_kbps,
            stream_cache_bitrate_kbps: if hls_cache_active {
                network_stats.cache_bitrate_kbps
            } else {
                None
            },
            stream_fps: network_stats.push_fps,
            stream_frame: network_stats.push_frame,
            stream_total_bytes: network_stats.push_total_bytes,
            stream_cache_total_bytes: if hls_cache_active {
                network_stats.cache_total_bytes
            } else {
                0
            },
            hls_cache_active,
        }),
        message: None,
    })
}

// Refresh YouTube status (fetch fresh data and update cache)
pub async fn refresh_youtube_status() -> Json<ApiResponse<()>> {
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

    if cfg.youtube.channel_id.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some("YouTube channel not configured".to_string()),
        });
    }

    match refresh_youtube_status_cache_with_config(&cfg).await {
        Ok(()) => Json(ApiResponse {
            success: true,
            data: Some(()),
            message: Some("YouTube status refreshed".to_string()),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to get YouTube status: {}", e)),
        }),
    }
}

// Refresh Twitch status (fetch fresh data and update cache)
pub async fn refresh_twitch_status() -> Json<ApiResponse<()>> {
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

    if cfg.twitch.channel_id.is_empty() {
        return Json(ApiResponse {
            success: false,
            data: None,
            message: Some("Twitch channel not configured".to_string()),
        });
    }

    match refresh_twitch_status_cache_with_config(&cfg).await {
        Ok(()) => Json(ApiResponse {
            success: true,
            data: Some(()),
            message: Some("Twitch status refreshed".to_string()),
        }),
        Err(e) => Json(ApiResponse {
            success: false,
            data: None,
            message: Some(format!("Failed to get Twitch status: {}", e)),
        }),
    }
}
