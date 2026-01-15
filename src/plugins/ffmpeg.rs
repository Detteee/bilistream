use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

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
}

use std::sync::atomic::AtomicBool;

// Track if ffmpeg was stopped manually (e.g., via restart button)
static MANUAL_STOP: AtomicBool = AtomicBool::new(false);

// Track if a manual restart was requested (force immediate restart even if stream is live)
static MANUAL_RESTART: AtomicBool = AtomicBool::new(false);

// Represents a managed ffmpeg process
pub struct FfmpegProcess {
    child: Child,
    pid: Option<u32>,
}

impl FfmpegProcess {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
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

// Check if ffmpeg has made progress recently (within timeout seconds)
// This checks both: 1) if stats are being reported, 2) if stream time is progressing
pub async fn is_ffmpeg_stuck(timeout_secs: u64) -> bool {
    let last_progress = LAST_PROGRESS_TIME.load(Ordering::Relaxed);
    if last_progress == 0 {
        // No progress recorded yet, not stuck
        return false;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;

    // Check if we're getting stats updates
    let stats_elapsed = now.saturating_sub(last_progress);
    if stats_elapsed > timeout_secs as u32 {
        return true; // No stats for too long
    }

    // Check if stream time is progressing (only after initial startup)
    let last_stream_update = LAST_STREAM_TIME_UPDATE.load(Ordering::Relaxed);
    if last_stream_update > 0 {
        let stream_time_elapsed = now.saturating_sub(last_stream_update);

        // If stream time hasn't progressed for 10 seconds, stream has likely ended
        if stream_time_elapsed > 10 {
            tracing::warn!(
                "Stream time frozen for {} seconds, stream likely ended",
                stream_time_elapsed
            );
            return true;
        }
    }

    false
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
            tracing::info!("🛑 Stopping ffmpeg process (PID: {})", pid_value);
        }

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
        tracing::info!("✅ ffmpeg process stopped");
    } else {
        tracing::info!("No ffmpeg process to stop");
    }

    // Clear speed and progress time when ffmpeg stops (lock-free write)
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    LAST_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
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

        // Quick parse for speed and time values (for stuck detection)
        for part in stats_from_time.split_whitespace() {
            if let Some(value) = part.strip_prefix("time=") {
                time_value = parse_time_to_seconds(value);
            } else if let Some(value) = part.strip_prefix("speed=") {
                // Parse speed value (remove 'x' suffix if present)
                let clean_value = value.trim_end_matches('x');
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

/// Spawns and supervises an ffmpeg process with output monitoring
pub async fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: String,
) {
    // Check if already running
    if is_ffmpeg_running().await {
        tracing::debug!("ffmpeg already running, skipping spawn");
        return;
    }

    let rtmp_url_key = format!("{}{}", rtmp_url, rtmp_key);

    let mut cmd = Command::new(get_ffmpeg_command());

    // Hide console window on Windows
    #[cfg(target_os = "windows")]
    {
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    // Network optimization
    if let Some(proxy) = proxy {
        cmd.arg("-http_proxy").arg(proxy);
    }

    // Input options - optimized for stability
    // .arg("-multiple_requests")
    // .arg("1") // Use multiple HTTP requests for segments
    cmd.arg("-thread_queue_size")
        .arg("4096")
        .arg("-re") // Read input at native frame rate
        .arg("-analyzeduration")
        .arg("5000000") // 5 seconds
        .arg("-probesize")
        .arg("5000000")
        .arg("-fflags")
        .arg("+genpts+discardcorrupt") // Generate PTS and discard corrupt packets
        // Input file
        .arg("-i")
        .arg(m3u8_url)
        // Output options - stream copy
        .arg("-c")
        .arg("copy") // Stream copy without re-encoding
        // .arg("-copyts") // Copy input timestamps
        .arg("-start_at_zero") // Start timestamps at zero
        .arg("-avoid_negative_ts")
        .arg("make_zero") // Shift timestamps to avoid negative values
        .arg("-max_interleave_delta")
        .arg("0") // Reduce muxing delay for lower latency
        .arg("-rtmp_buffer")
        .arg("5000k")
        .arg("-bufsize")
        .arg("5000k")
        .arg("-max_muxing_queue_size")
        .arg("8192") // Limit muxing queue to prevent memory issues
        .arg("-rtmp_live")
        .arg("1")
        // FLV/RTMP output
        .arg("-f")
        .arg("flv")
        .arg("-flvflags")
        .arg("no_duration_filesize") // Skip duration/filesize metadata for live streaming
        .arg(rtmp_url_key)
        .arg("-stats")
        .arg("-loglevel")
        .arg(&log_level);

    // Capture stdout and stderr
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            tracing::info!("🚀 ffmpeg process started (PID: {:?})", pid);

            // Set high priority for stable streaming
            if let Some(pid_value) = pid {
                set_high_priority(pid_value);
            }

            // Capture stderr for monitoring
            if let Some(stderr) = child.stderr.take() {
                let log_level_clone = log_level.clone();
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
                                if line_buffer.starts_with("frame=") || line_buffer.contains("fps=")
                                {
                                    // Extract only fps, time, bitrate, speed
                                    if let Some((compact, speed, stream_time)) =
                                        extract_compact_stats(&line_buffer)
                                    {
                                        eprint!("\r{:<50}", compact);
                                        let _ = std::io::stderr().flush();

                                        // Update global speed if available (lock-free atomic write)
                                        if let Some(s) = speed {
                                            FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
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
                                    if line_buffer.contains("error")
                                        || line_buffer.contains("Error")
                                    {
                                        tracing::error!("ffmpeg: {}", line_buffer);
                                    } else if line_buffer.contains("warning")
                                        || line_buffer.contains("Warning")
                                    {
                                        tracing::warn!("ffmpeg: {}", line_buffer);
                                    } else if line_buffer.starts_with("frame=")
                                        || line_buffer.contains("fps=")
                                    {
                                        // Final stats line with newline
                                        if let Some((compact, speed, stream_time)) =
                                            extract_compact_stats(&line_buffer)
                                        {
                                            eprintln!("\r{:<50}", compact);

                                            // Update global speed if available (lock-free atomic write)
                                            if let Some(s) = speed {
                                                FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
                                            }
                                            // Update stream time tracking
                                            if let Some(t) = stream_time {
                                                update_stream_time(t);
                                            }
                                            // Update progress time whenever we get stats
                                            update_progress_time();
                                        }
                                    } else if log_level_clone == "debug"
                                        || log_level_clone == "info"
                                    {
                                        tracing::debug!("ffmpeg: {}", line_buffer);
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

            // Store the process in supervisor
            let process = FfmpegProcess { child, pid };
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            *supervisor = Some(process);

            // Initialize progress time when ffmpeg starts
            update_progress_time();

            // Spawn timeout monitoring task (15 secs timeout)
            tokio::spawn(async {
                monitor_ffmpeg_timeout(15).await;
            });

            // tracing::info!("✅ ffmpeg process supervision started");
        }
        Err(e) => {
            tracing::error!("❌ Failed to spawn ffmpeg: {}", e);
        }
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
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    // Process has exited, remove it from supervisor
                    drop(supervisor);
                    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
                    supervisor.take();

                    if let Some(code) = status.code() {
                        tracing::info!("ffmpeg exited with status code: {}", code);
                    } else {
                        tracing::info!("ffmpeg terminated by signal");
                    }
                    return Some(status);
                }
                Ok(None) => {
                    // Process is still running, release lock and wait a bit
                    drop(supervisor);
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    tracing::error!("Failed to check ffmpeg status: {}", e);
                    drop(supervisor);
                    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
                    supervisor.take();
                    return None;
                }
            }
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

        // Check if ffmpeg is stuck (no progress for timeout_secs)
        if is_ffmpeg_stuck(timeout_secs).await {
            tracing::error!(
                "⚠️ ffmpeg appears stuck (no progress for {} seconds), killing process",
                timeout_secs
            );
            stop_ffmpeg_internal(false).await;
            break;
        }

        // Check every 5 seconds
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}
