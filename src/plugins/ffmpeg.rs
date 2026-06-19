use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
const DETACHED_PROCESS: u32 = 0x0000_0008;

#[cfg(target_os = "windows")]
fn configure_no_window(cmd: &mut Command) {
    #[allow(unused_imports)]
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

// Stuck-detection thresholds for live HLS restreaming.
const LOW_SPEED_THRESHOLD: f32 = 0.98;
const LOW_SPEED_TIMEOUT_SECS: u32 = 60;
const STREAM_TIME_FROZEN_SECS: u32 = 10;
const STARTUP_NO_STATS_TIMEOUT_SECS: u64 = 30;
const CACHE_PLAYLIST_WAIT_SECS: u64 = 30;

const HLS_CACHE_SEGMENT_SECS: u32 = 2;
const HLS_CACHE_LIST_SIZE: u32 = 30;

#[derive(Clone, Copy)]
pub struct FfmpegCacheOptions {
    pub enabled: bool,
    pub latency_secs: u64,
}

impl FfmpegCacheOptions {
    fn latency_secs(&self) -> u64 {
        self.latency_secs.clamp(1, 60)
    }
}

fn cache_startup_timeout_secs(latency_secs: u64) -> u64 {
    latency_secs + CACHE_PLAYLIST_WAIT_SECS
}

#[derive(Debug)]
enum StuckReason {
    NoStats { elapsed_secs: u32 },
    StreamTimeFrozen { elapsed_secs: u32 },
    LowSpeed { elapsed_secs: u32 },
}

// Global process supervisor
lazy_static::lazy_static! {
    static ref FFMPEG_SUPERVISOR: Arc<Mutex<Option<FfmpegProcess>>> = Arc::new(Mutex::new(None));
    // Use atomic for lock-free speed updates (stored as f32 bits)
    static ref FFMPEG_SPEED: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track last progress time for timeout detection (stored as Unix timestamp in seconds)
    static ref LAST_PROGRESS_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track last reported stream time from ffmpeg (stored as seconds, converted from HH:MM:SS.ms)
    static ref LAST_STREAM_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when stream time last changed (Unix timestamp in seconds)
    static ref LAST_STREAM_TIME_UPDATE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when speed first dropped below LOW_SPEED_THRESHOLD (0 = speed is OK)
    static ref LOW_SPEED_SINCE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
}

use std::sync::atomic::AtomicBool;

// Track if ffmpeg was stopped manually (e.g., via restart button)
static MANUAL_STOP: AtomicBool = AtomicBool::new(false);

// Track if a manual restart was requested (force immediate restart even if stream is live)
static MANUAL_RESTART: AtomicBool = AtomicBool::new(false);

// Represents a managed ffmpeg process
pub struct FfmpegProcess {
    children: Vec<Child>,
    pid: Option<u32>,
    cache_dir: Option<PathBuf>,
}

impl FfmpegProcess {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(child) = self.children.first_mut() {
            child.wait().await
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no ffmpeg child process",
            ))
        }
    }

    pub async fn kill(&mut self) -> std::io::Result<()> {
        let mut result = Ok(());
        for child in &mut self.children {
            if let Err(e) = child.kill().await {
                result = Err(e);
            }
        }
        result
    }
}

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

// Set high priority for ffmpeg process to ensure stable streaming
fn set_high_priority(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        // On Linux, use renice to set nice value to -10 (higher priority)
        // Nice values range from -20 (highest) to 19 (lowest), default is 0
        let status = std::process::Command::new("renice")
            .arg("-n")
            .arg("-10")
            .arg("-p")
            .arg(pid.to_string())
            .output();

        match status {
            Ok(output) if output.status.success() => {
                // tracing::info!("✅ Set ffmpeg process priority to high (nice -10)");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("⚠️ Failed to set process priority: {}", stderr.trim());
                tracing::info!(
                    "💡 Tip: Run with sudo or set CAP_SYS_NICE capability for better performance"
                );
            }
            Err(e) => {
                tracing::warn!("⚠️ Could not set process priority: {}", e);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // On Windows, use wmic to set priority to "high priority"
        // Priority classes: realtime, high, abovenormal, normal, belownormal, low
        let status = std::process::Command::new("wmic")
            .arg("process")
            .arg("where")
            .arg(format!("ProcessId={}", pid))
            .arg("CALL")
            .arg("setpriority")
            .arg("128") // 128 = High priority
            .output();

        match status {
            Ok(output) if output.status.success() => {
                // tracing::info!("✅ Set ffmpeg process priority to high");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("⚠️ Failed to set process priority: {}", stderr.trim());
            }
            Err(e) => {
                tracing::warn!("⚠️ Could not set process priority: {}", e);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // On macOS, use renice similar to Linux
        let status = std::process::Command::new("renice")
            .arg("-n")
            .arg("-10")
            .arg("-p")
            .arg(pid.to_string())
            .output();

        match status {
            Ok(output) if output.status.success() => {
                // tracing::info!("✅ Set ffmpeg process priority to high (nice -10)");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("⚠️ Failed to set process priority: {}", stderr.trim());
                tracing::info!("💡 Tip: Run with sudo for better performance");
            }
            Err(e) => {
                tracing::warn!("⚠️ Could not set process priority: {}", e);
            }
        }
    }
}

// Check if ffmpeg is running via supervisor
pub async fn is_ffmpeg_running() -> bool {
    let supervisor = FFMPEG_SUPERVISOR.lock().await;
    supervisor.is_some()
}

// Check if ffmpeg was stopped manually
pub fn was_manual_stop() -> bool {
    MANUAL_STOP.load(Ordering::SeqCst)
}

// Clear manual stop flag
pub fn clear_manual_stop() {
    MANUAL_STOP.store(false, Ordering::SeqCst);
}

// Set manual restart flag (force immediate restart)
pub fn set_manual_restart() {
    MANUAL_RESTART.store(true, Ordering::SeqCst);
}

// Check if manual restart was requested
pub fn was_manual_restart() -> bool {
    MANUAL_RESTART.load(Ordering::SeqCst)
}

// Clear manual restart flag
pub fn clear_manual_restart() {
    MANUAL_RESTART.store(false, Ordering::SeqCst);
}

// Get current ffmpeg speed (lock-free read)
pub async fn get_ffmpeg_speed() -> Option<f32> {
    let bits = FFMPEG_SPEED.load(Ordering::Relaxed);
    if bits == 0 {
        None
    } else {
        Some(f32::from_bits(bits))
    }
}

// Update last progress time (lock-free write)
fn update_progress_time() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    LAST_PROGRESS_TIME.store(now, Ordering::Relaxed);
}

// Update stream time tracking (lock-free write)
fn update_stream_time(stream_time_secs: u32) {
    let last_time = LAST_STREAM_TIME.load(Ordering::Relaxed);

    // Only update if time has actually progressed
    if stream_time_secs > last_time {
        LAST_STREAM_TIME.store(stream_time_secs, Ordering::Relaxed);

        // Update the timestamp when stream time last changed
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;
        LAST_STREAM_TIME_UPDATE.store(now, Ordering::Relaxed);
    }
}

// Update low-speed tracking: record when speed first drops below threshold, clear when it recovers
fn update_speed_tracking(speed: f32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    if speed < LOW_SPEED_THRESHOLD {
        // Only set the timestamp if not already tracking a low-speed period
        LOW_SPEED_SINCE
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
    } else {
        LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
    }
}

// Check if ffmpeg has made progress recently (within timeout seconds)
// This checks: 1) stats updates, 2) stream time progression, 3) sustained low speed
fn check_ffmpeg_stuck(timeout_secs: u64) -> Option<StuckReason> {
    let last_progress = LAST_PROGRESS_TIME.load(Ordering::Relaxed);
    if last_progress == 0 {
        // No progress recorded yet, not stuck
        return None;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;

    // Check if we're getting stats updates
    let stats_elapsed = now.saturating_sub(last_progress);
    if stats_elapsed > timeout_secs as u32 {
        return Some(StuckReason::NoStats {
            elapsed_secs: stats_elapsed,
        });
    }

    // Check if stream time is progressing (only after initial startup)
    let last_stream_update = LAST_STREAM_TIME_UPDATE.load(Ordering::Relaxed);
    if last_stream_update > 0 {
        let stream_time_elapsed = now.saturating_sub(last_stream_update);

        if stream_time_elapsed > STREAM_TIME_FROZEN_SECS {
            return Some(StuckReason::StreamTimeFrozen {
                elapsed_secs: stream_time_elapsed,
            });
        }
    }

    let low_speed_since = LOW_SPEED_SINCE.load(Ordering::Relaxed);
    if low_speed_since > 0 {
        let low_speed_elapsed = now.saturating_sub(low_speed_since);
        if low_speed_elapsed > LOW_SPEED_TIMEOUT_SECS {
            return Some(StuckReason::LowSpeed {
                elapsed_secs: low_speed_elapsed,
            });
        }
    }

    None
}

pub async fn is_ffmpeg_stuck(timeout_secs: u64) -> bool {
    check_ffmpeg_stuck(timeout_secs).is_some()
}

/// Stops the supervised ffmpeg process
pub async fn stop_ffmpeg() {
    stop_ffmpeg_internal(true).await;
}

/// Internal stop function with manual flag
async fn stop_ffmpeg_internal(manual: bool) {
    if manual {
        MANUAL_STOP.store(true, Ordering::SeqCst);
    }

    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
    if let Some(mut process) = supervisor.take() {
        let pid = process.pid();
        if let Some(pid_value) = pid {
            tracing::info!("🛑 Stopping ffmpeg process group (main PID: {})", pid_value);
        }
        let cache_dir = process.cache_dir.clone();

        // Try tokio kill first
        match process.kill().await {
            Ok(_) => {
                // Successfully killed via tokio
            }
            Err(e) => {
                tracing::warn!("⚠️ Tokio kill failed: {}, trying system kill", e);

                // Fallback to system kill command
                if let Some(pid_value) = pid {
                    #[cfg(unix)]
                    {
                        let kill_result = std::process::Command::new("kill")
                            .arg("-9")
                            .arg(pid_value.to_string())
                            .output();

                        match kill_result {
                            Ok(output) if output.status.success() => {
                                // Successfully killed via system kill
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                tracing::error!("❌ System kill failed: {}", stderr);
                            }
                            Err(e) => {
                                tracing::error!("❌ Failed to execute kill command: {}", e);
                            }
                        }
                    }

                    #[cfg(windows)]
                    {
                        let kill_result = std::process::Command::new("taskkill")
                            .arg("/F")
                            .arg("/PID")
                            .arg(pid_value.to_string())
                            .output();

                        match kill_result {
                            Ok(output) if output.status.success() => {
                                // Successfully killed via taskkill
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                tracing::error!("❌ Taskkill failed: {}", stderr);
                            }
                            Err(e) => {
                                tracing::error!("❌ Failed to execute taskkill: {}", e);
                            }
                        }
                    }
                }
            }
        }

        // Wait a bit for process to actually terminate
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if let Some(cache_dir) = cache_dir {
            if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
                tracing::warn!(
                    "⚠️ Failed to remove HLS cache directory {}: {}",
                    cache_dir.display(),
                    e
                );
            }
        }
        tracing::info!("✅ ffmpeg process stopped");
    } else {
        tracing::info!("No ffmpeg process to stop");
    }

    // Clear speed and progress time when ffmpeg stops (lock-free write)
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    LAST_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
}
/// Parse time string (HH:MM:SS.ms) to seconds
fn parse_time_to_seconds(time_str: &str) -> Option<u32> {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() != 3 {
        return None;
    }

    let hours: u32 = parts[0].parse().ok()?;
    let minutes: u32 = parts[1].parse().ok()?;
    let seconds: f32 = parts[2].parse().ok()?;

    Some(hours * 3600 + minutes * 60 + seconds as u32)
}

/// Extract simplified stats from ffmpeg output line starting from "time="
/// Optimized for speed - extracts everything from "time=" onwards for webui and stuck detection
/// Returns (stats_from_time, parsed_speed, parsed_time_in_seconds)
fn extract_compact_stats(line: &str) -> Option<(String, Option<f32>, Option<u32>)> {
    // Find "time=" position for fast extraction
    if let Some(time_pos) = line.find("time=") {
        let stats_from_time = &line[time_pos..];

        let mut speed_value: Option<f32> = None;
        let mut time_value: Option<u32> = None;
        let parts: Vec<&str> = stats_from_time.split_whitespace().collect();

        // Quick parse for speed and time values (for stuck detection)
        for (idx, part) in parts.iter().enumerate() {
            if let Some(value) = part.strip_prefix("time=") {
                time_value = parse_time_to_seconds(value);
            } else if let Some(value) = part.strip_prefix("speed=") {
                // Handle both formats:
                // - "speed=0.99x" (single token)
                // - "speed=   1x" (value is in next token because of padding)
                let speed_token = if value.is_empty() {
                    parts.get(idx + 1).copied().unwrap_or("")
                } else {
                    value
                };
                let clean_value = speed_token.trim_end_matches('x');
                if let Ok(parsed) = clean_value.parse::<f32>() {
                    speed_value = Some(parsed);
                }
            }
        }

        Some((stats_from_time.to_string(), speed_value, time_value))
    } else {
        None
    }
}

/// Fast extraction of stats from "time=" onwards for console display
/// Returns raw string starting from "time=" or None if not found
/// This is optimized for speed - no parsing, just string slicing
pub fn extract_stats_from_time(line: &str) -> Option<String> {
    line.find("time=").map(|pos| line[pos..].to_string())
}

fn create_hls_cache_dir() -> std::io::Result<PathBuf> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let cache_dir = std::env::temp_dir().join(format!(
        "bilistream-hls-cache-{}-{}",
        std::process::id(),
        now_ms
    ));
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

fn spawn_ffmpeg_stderr_monitor(
    stderr: tokio::process::ChildStderr,
    log_level: String,
    process_name: &'static str,
) {
    tokio::spawn(async move {
        use std::io::Write;
        use tokio::io::AsyncReadExt;

        let mut stderr = stderr;
        let mut buffer = vec![0u8; 8192];
        let mut line_buffer = String::new();

        while let Ok(n) = stderr.read(&mut buffer).await {
            if n == 0 {
                break;
            }

            let chunk = String::from_utf8_lossy(&buffer[..n]);

            for ch in chunk.chars() {
                if ch == '\r' {
                    // Carriage return - stats update
                    if line_buffer.starts_with("frame=") || line_buffer.contains("fps=") {
                        // Extract only fps, time, bitrate, speed
                        if let Some((compact, speed, stream_time)) =
                            extract_compact_stats(&line_buffer)
                        {
                            eprint!("\r{:<50}", compact);
                            let _ = std::io::stderr().flush();

                            // Update global speed if available (lock-free atomic write)
                            if let Some(s) = speed {
                                FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
                                update_speed_tracking(s);
                            }
                            // Update stream time tracking
                            if let Some(t) = stream_time {
                                update_stream_time(t);
                            }
                            // Update progress time whenever we get stats
                            update_progress_time();
                        }
                    }
                    line_buffer.clear();
                } else if ch == '\n' {
                    // Newline - complete message
                    if !line_buffer.is_empty() {
                        if line_buffer.contains("error") || line_buffer.contains("Error") {
                            tracing::error!("{}: {}", process_name, line_buffer);
                        } else if line_buffer.contains("warning") || line_buffer.contains("Warning")
                        {
                            tracing::warn!("{}: {}", process_name, line_buffer);
                        } else if line_buffer.starts_with("frame=") || line_buffer.contains("fps=")
                        {
                            // Final stats line with newline
                            if let Some((compact, speed, stream_time)) =
                                extract_compact_stats(&line_buffer)
                            {
                                eprintln!("\r{:<50}", compact);

                                // Update global speed if available (lock-free atomic write)
                                if let Some(s) = speed {
                                    FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
                                    update_speed_tracking(s);
                                }
                                // Update stream time tracking
                                if let Some(t) = stream_time {
                                    update_stream_time(t);
                                }
                                // Update progress time whenever we get stats
                                update_progress_time();
                            }
                        } else if log_level == "debug" || log_level == "info" {
                            tracing::debug!("{}: {}", process_name, line_buffer);
                        }
                    }
                    line_buffer.clear();
                } else {
                    line_buffer.push(ch);
                }
            }
        }
    });
}

fn append_crop_or_copy(cmd: &mut Command, crop: Option<(u32, u32, u32, u32)>) {
    if let Some((width, height, x, y)) = crop {
        tracing::info!("🎬 Applying crop filter: {}:{}:{}:{}", width, height, x, y);
        cmd.arg("-vf")
            .arg(format!("crop={}:{}:{}:{}", width, height, x, y))
            .arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("veryfast")
            .arg("-c:a")
            .arg("copy");
    } else {
        cmd.arg("-c").arg("copy");
    }
}

fn reset_ffmpeg_tracking_state() {
    update_progress_time();
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
}

fn start_ffmpeg_timeout_monitor(timeout_secs: u64) {
    tokio::spawn(async move {
        monitor_ffmpeg_timeout(timeout_secs).await;
    });
}

async fn spawn_direct_ffmpeg(
    rtmp_url_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>,
) {
    let ffmpeg_cmd = get_ffmpeg_command();
    tracing::info!("⏱️ HLS cache disabled: direct restream");

    let mut cmd = Command::new(&ffmpeg_cmd);
    #[cfg(target_os = "windows")]
    configure_no_window(&mut cmd);

    if let Some(proxy_url) = proxy {
        cmd.arg("-http_proxy").arg(proxy_url);
    }

    cmd.arg("-nostdin")
        .arg("-stats")
        .arg("-loglevel")
        .arg(&log_level)
        .arg("-multiple_requests")
        .arg("1")
        .arg("-thread_queue_size")
        .arg("2048")
        .arg("-re")
        .arg("-fflags")
        .arg("+genpts")
        .arg("-i")
        .arg(m3u8_url);

    append_crop_or_copy(&mut cmd, crop);

    cmd.arg("-max_muxing_queue_size")
        .arg("8192")
        .arg("-f")
        .arg("flv")
        .arg(rtmp_url_key)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            tracing::info!("🚀 ffmpeg process started (PID: {:?})", pid);

            if let Some(pid_value) = pid {
                set_high_priority(pid_value);
            }

            if let Some(stderr) = child.stderr.take() {
                spawn_ffmpeg_stderr_monitor(stderr, log_level, "ffmpeg");
            }

            let process = FfmpegProcess {
                children: vec![child],
                pid,
                cache_dir: None,
            };
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            *supervisor = Some(process);

            reset_ffmpeg_tracking_state();
            start_ffmpeg_timeout_monitor(STARTUP_NO_STATS_TIMEOUT_SECS);
        }
        Err(e) => {
            tracing::error!("❌ Failed to spawn ffmpeg: {}", e);
        }
    }
}

async fn spawn_cached_ffmpeg(
    rtmp_url_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>,
    latency_secs: u64,
) {
    let cache_dir = match create_hls_cache_dir() {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("❌ Failed to create HLS cache directory: {}", e);
            return;
        }
    };
    let playlist_path = cache_dir.join("index.m3u8");
    let segment_pattern = cache_dir.join("segment_%06d.ts");
    let ffmpeg_cmd = get_ffmpeg_command();

    tracing::info!(
        "⏱️ HLS cache enabled: {}s input-to-output latency ({})",
        latency_secs,
        playlist_path.display()
    );

    let mut cache_cmd = Command::new(&ffmpeg_cmd);
    #[cfg(target_os = "windows")]
    configure_no_window(&mut cache_cmd);

    if let Some(proxy_url) = proxy.clone() {
        cache_cmd.arg("-http_proxy").arg(proxy_url);
    }

    cache_cmd
        .arg("-nostdin")
        .arg("-stats")
        .arg("-loglevel")
        .arg("warning")
        .arg("-multiple_requests")
        .arg("1")
        .arg("-thread_queue_size")
        .arg("2048")
        .arg("-rtbufsize")
        .arg("100M")
        .arg("-fflags")
        .arg("+genpts")
        .arg("-i")
        .arg(m3u8_url)
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(HLS_CACHE_SEGMENT_SECS.to_string())
        .arg("-hls_list_size")
        .arg(HLS_CACHE_LIST_SIZE.to_string())
        .arg("-hls_delete_threshold")
        .arg("10")
        .arg("-hls_flags")
        .arg("delete_segments+append_list+omit_endlist")
        .arg("-hls_segment_filename")
        .arg(segment_pattern)
        .arg(&playlist_path);

    cache_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    match cache_cmd.spawn() {
        Ok(mut cache_child) => {
            let cache_pid = cache_child.id();
            tracing::info!("🚀 ffmpeg HLS cache writer started (PID: {:?})", cache_pid);

            if let Some(pid_value) = cache_pid {
                set_high_priority(pid_value);
            }

            if let Some(stderr) = cache_child.stderr.take() {
                spawn_ffmpeg_stderr_monitor(stderr, log_level.clone(), "ffmpeg cache writer");
            }

            let process = FfmpegProcess {
                children: vec![cache_child],
                pid: cache_pid,
                cache_dir: Some(cache_dir.clone()),
            };
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            *supervisor = Some(process);
            drop(supervisor);

            let reader_playlist_path = playlist_path.clone();
            let reader_ffmpeg_cmd = ffmpeg_cmd.clone();
            let reader_log_level = log_level.clone();
            let reader_crop = crop;
            let reader_rtmp_url_key = rtmp_url_key.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(latency_secs)).await;

                let mut waited_ms = 0;
                let wait_limit_ms = CACHE_PLAYLIST_WAIT_SECS * 1000;
                while tokio::fs::metadata(&reader_playlist_path).await.is_err()
                    && waited_ms < wait_limit_ms
                {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    waited_ms += 500;
                }

                if tokio::fs::metadata(&reader_playlist_path).await.is_err() {
                    tracing::error!(
                        "❌ HLS cache playlist was not created after {}s: {}",
                        latency_secs + CACHE_PLAYLIST_WAIT_SECS,
                        reader_playlist_path.display()
                    );
                    stop_ffmpeg_internal(false).await;
                    return;
                }

                let mut reader_cmd = Command::new(reader_ffmpeg_cmd);
                #[cfg(target_os = "windows")]
                configure_no_window(&mut reader_cmd);

                reader_cmd
                    .arg("-nostdin")
                    .arg("-stats")
                    .arg("-loglevel")
                    .arg(&reader_log_level)
                    .arg("-re")
                    .arg("-fflags")
                    .arg("+genpts")
                    .arg("-live_start_index")
                    .arg("0")
                    .arg("-i")
                    .arg(&reader_playlist_path);

                append_crop_or_copy(&mut reader_cmd, reader_crop);

                reader_cmd
                    .arg("-max_muxing_queue_size")
                    .arg("8192")
                    .arg("-f")
                    .arg("flv")
                    .arg(reader_rtmp_url_key)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                match reader_cmd.spawn() {
                    Ok(mut reader_child) => {
                        let reader_pid = reader_child.id();
                        tracing::info!(
                            "🚀 ffmpeg delayed RTMP reader started (PID: {:?})",
                            reader_pid
                        );
                        if let Some(pid_value) = reader_pid {
                            set_high_priority(pid_value);
                        }
                        if let Some(stderr) = reader_child.stderr.take() {
                            spawn_ffmpeg_stderr_monitor(
                                stderr,
                                reader_log_level,
                                "ffmpeg delayed reader",
                            );
                        }

                        let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
                        if let Some(process) = supervisor.as_mut() {
                            process.children.push(reader_child);
                        }
                    }
                    Err(e) => {
                        tracing::error!("❌ Failed to spawn delayed RTMP ffmpeg: {}", e);
                        stop_ffmpeg_internal(false).await;
                    }
                }
            });

            reset_ffmpeg_tracking_state();
            start_ffmpeg_timeout_monitor(cache_startup_timeout_secs(latency_secs));
        }
        Err(e) => {
            tracing::error!("❌ Failed to spawn ffmpeg cache writer: {}", e);
        }
    }
}

/// Spawns and supervises an ffmpeg process with output monitoring
pub async fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>, // (width, height, x, y)
    cache: FfmpegCacheOptions,
) {
    // Check if already running
    if is_ffmpeg_running().await {
        return;
    }

    let rtmp_url_key = format!("{}{}", rtmp_url, rtmp_key);
    let latency_secs = cache.latency_secs();

    if cache.enabled {
        spawn_cached_ffmpeg(rtmp_url_key, m3u8_url, proxy, log_level, crop, latency_secs).await;
    } else {
        spawn_direct_ffmpeg(rtmp_url_key, m3u8_url, proxy, log_level, crop).await;
    }
}

/// Wait for the ffmpeg process to exit and return the exit status
/// This function blocks until ffmpeg exits or is killed
pub async fn wait_ffmpeg() -> Option<std::process::ExitStatus> {
    // Poll to check if process is still running, allowing stop_ffmpeg to interrupt
    loop {
        let mut supervisor = FFMPEG_SUPERVISOR.lock().await;

        if let Some(process) = supervisor.as_mut() {
            // Check if process has exited without blocking
            for child in &mut process.children {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if let Some(code) = status.code() {
                            tracing::info!("ffmpeg exited with status code: {}", code);
                        } else {
                            tracing::info!("ffmpeg terminated by signal");
                        }

                        drop(supervisor);
                        stop_ffmpeg_internal(false).await;
                        return Some(status);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Failed to check ffmpeg status: {}", e);
                        drop(supervisor);
                        stop_ffmpeg_internal(false).await;
                        return None;
                    }
                }
            }

            // Process is still running, release lock and wait a bit
            drop(supervisor);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        } else {
            // Process was removed (killed by stop_ffmpeg)
            return None;
        }
    }
}

/// Background task to monitor ffmpeg timeout and kill if stuck
async fn monitor_ffmpeg_timeout(timeout_secs: u64) {
    loop {
        // Check if ffmpeg is still running
        if !is_ffmpeg_running().await {
            // Process exited, stop monitoring
            break;
        }

        if let Some(reason) = check_ffmpeg_stuck(timeout_secs) {
            match reason {
                StuckReason::NoStats { elapsed_secs } => {
                    tracing::error!(
                        "⚠️ ffmpeg appears stuck (no stats for {} seconds), killing process",
                        elapsed_secs
                    );
                }
                StuckReason::StreamTimeFrozen { elapsed_secs } => {
                    tracing::error!(
                        "⚠️ ffmpeg appears stuck (stream time frozen for {} seconds), killing process",
                        elapsed_secs
                    );
                }
                StuckReason::LowSpeed { elapsed_secs } => {
                    tracing::error!(
                        "⚠️ ffmpeg appears stuck (speed below {} for {} seconds), killing process",
                        LOW_SPEED_THRESHOLD,
                        elapsed_secs
                    );
                }
            }
            stop_ffmpeg_internal(false).await;
            break;
        }

        // Check every 5 seconds
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}
