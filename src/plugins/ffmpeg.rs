use std::process::Command;

// Helper function to get ffmpeg command path
fn get_ffmpeg_command() -> String {
    if cfg!(target_os = "windows") {
        // On Windows, check if ffmpeg.exe exists in the executable directory
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_ffmpeg = exe_dir.join("ffmpeg.exe");
                if local_ffmpeg.exists() {
                    return local_ffmpeg.to_string_lossy().to_string();
                }
            }
        }
        "ffmpeg.exe".to_string()
    } else {
        "ffmpeg".to_string()
    }
}

// if ffmpeg is already running
pub fn is_ffmpeg_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        // On Windows, use tasklist
        let output = match Command::new("tasklist")
            .arg("/FI")
            .arg("IMAGENAME eq ffmpeg.exe")
            .output()
        {
            Ok(output) => output,
            Err(_) => return false,
        };

        if output.status.success() {
            let process_info = String::from_utf8_lossy(&output.stdout);
            return process_info.contains("ffmpeg.exe");
        }
        false
    }

    #[cfg(not(target_os = "windows"))]
    {
        // On Unix/Linux, use pgrep
        let output = match Command::new("pgrep").arg("-af").arg("ffmpeg").output() {
            Ok(output) => output,
            Err(_) => return false,
        };

        if output.status.success() {
            let process_info = String::from_utf8_lossy(&output.stdout);
            if process_info.contains("ffmpeg -re") {
                return true;
            }
        }
        false
    }
}

/// Stops all running ffmpeg processes
pub fn stop_ffmpeg() {
    tracing::info!("🛑 Stopping ffmpeg processes...");

    #[cfg(target_os = "windows")]
    {
        match Command::new("taskkill")
            .arg("/F")
            .arg("/IM")
            .arg("ffmpeg.exe")
            .status()
        {
            Ok(status) => {
                if status.success() {
                    tracing::info!("✅ ffmpeg processes stopped successfully");
                } else {
                    tracing::warn!("⚠️ taskkill returned non-zero status (no processes found?)");
                }
            }
            Err(e) => {
                tracing::error!("❌ Failed to stop ffmpeg: {}", e);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        match Command::new("pkill").arg("-f").arg("ffmpeg -re").status() {
            Ok(status) => {
                if status.success() {
                    tracing::info!("✅ ffmpeg processes stopped successfully");
                } else {
                    tracing::warn!("⚠️ pkill returned non-zero status (no processes found?)");
                }
            }
            Err(e) => {
                tracing::error!("❌ Failed to stop ffmpeg: {}", e);
            }
        }
    }
}
/// Executes the ffmpeg command with the provided parameters.
pub fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: &str,
) {
    if is_ffmpeg_running() {
        return;
    }
    let rtmp_url_key = format!("{}{}", rtmp_url, rtmp_key);
    // name the ffmpeg process as ffmpeg-platform

    let mut child = Command::new(get_ffmpeg_command());

    if let Some(proxy) = proxy {
        child.arg("-http_proxy").arg(proxy);
    }
    // Input options
    child
        .arg("-re") // Read input at native frame rate
        .arg("-thread_queue_size")
        .arg("40960k")
        .arg("-analyzeduration")
        .arg("4000000")
        // Input file
        .arg("-i")
        .arg(m3u8_url)
        // Output options
        .arg("-c")
        .arg("copy")
        // Frame and timestamp handling
        .arg("-fflags")
        .arg("+genpts+discardcorrupt")
        .arg("-max_delay")
        .arg("8000000")
        // Rate control
        .arg("-bufsize")
        .arg("10240k")
        .arg("-maxrate")
        .arg("20480k")
        // Force frame output
        // .arg("-vsync") // Important
        // .arg("passthrough") // Pass through timestamps without modification
        // RTMP settings
        .arg("-rtmp_buffer")
        .arg("10240k")
        .arg("-rtmp_live")
        .arg("live")
        // .arg("-max_muxing_queue_size")
        // .arg("81920")
        .arg("-f")
        .arg("flv")
        .arg(rtmp_url_key)
        // .arg("-stats_period")
        // .arg("2") // Update stats every 2 second
        .arg("-stats")
        .arg("-loglevel")
        .arg(log_level);

    match child.status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                tracing::info!("ffmpeg退出状态码: {}", code);
            } else {
                tracing::info!("ffmpeg被信号终止");
            }
        }
        Err(e) => tracing::error!("执行ffmpeg失败: {}", e),
    }
}
