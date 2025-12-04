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
}

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

// Check if ffmpeg has made progress recently (within timeout seconds)
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

    let elapsed = now.saturating_sub(last_progress);
    elapsed > timeout_secs as u32
}

/// Stops the supervised ffmpeg process
pub async fn stop_ffmpeg() {
    tracing::info!("🛑 Stopping ffmpeg process...");

    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
    if let Some(mut process) = supervisor.take() {
        let pid = process.pid();
        if let Some(pid_value) = pid {
            tracing::info!("Terminating ffmpeg process (PID: {})", pid_value);
        }

        // Try tokio kill first
        match process.kill().await {
            Ok(_) => {
                tracing::info!("✅ ffmpeg process killed via tokio");
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
                                tracing::info!("✅ ffmpeg process killed via system kill -9");
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
                                tracing::info!("✅ ffmpeg process killed via taskkill");
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
        tracing::info!("✅ ffmpeg process stopped successfully");
    } else {
        tracing::warn!("⚠️ No ffmpeg process to stop");
    }

    // Clear speed and progress time when ffmpeg stops (lock-free write)
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    LAST_PROGRESS_TIME.store(0, Ordering::Relaxed);
}
/// Extract compact stats from ffmpeg output line
/// Only shows: time, bitrate, speed (fps removed as it's often empty)
/// Returns (formatted_string, parsed_speed)
fn extract_compact_stats(line: &str) -> Option<(String, Option<f32>)> {
    let mut time = None;
    let mut bitrate = None;
    let mut speed = None;
    let mut speed_value: Option<f32> = None;

    // Parse the line for key=value pairs
    for part in line.split_whitespace() {
        if let Some(value) = part.strip_prefix("time=") {
            time = Some(value.to_string());
        } else if let Some(value) = part.strip_prefix("bitrate=") {
            bitrate = Some(value.to_string());
        } else if let Some(value) = part.strip_prefix("speed=") {
            speed = Some(value.to_string());
            // Parse speed value (remove 'x' suffix if present)
            let clean_value = value.trim_end_matches('x');
            if let Ok(parsed) = clean_value.parse::<f32>() {
                speed_value = Some(parsed);
            }
        }
    }

    // Build compact output
    if time.is_some() || bitrate.is_some() || speed.is_some() {
        let mut output = String::new();

        if let Some(t) = time {
            if !t.is_empty() {
                output.push_str(&format!("time={} ", t));
            }
        }
        if let Some(b) = bitrate {
            if !b.is_empty() {
                output.push_str(&format!("bitrate={} ", b));
            }
        }
        if let Some(s) = speed {
            if !s.is_empty() {
                output.push_str(&format!("speed={}", s));
            }
        }

        let trimmed = output.trim_end().to_string();
        if !trimmed.is_empty() {
            Some((trimmed, speed_value))
        } else {
            None
        }
    } else {
        None
    }
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

    // Network optimization
    if let Some(proxy) = proxy {
        cmd.arg("-http_proxy").arg(proxy);
    }

    // Input options - optimized for stability
    cmd.arg("-multiple_requests")
        .arg("1") // Use multiple HTTP requests for segments
        .arg("-re") // Read input at native frame rate
        .arg("-thread_queue_size")
        .arg("1024")
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
        .arg("-copyts") // Copy input timestamps
        .arg("-start_at_zero") // Start timestamps at zero
        .arg("-avoid_negative_ts")
        .arg("make_zero") // Shift timestamps to avoid negative values
        .arg("-max_interleave_delta")
        .arg("0") // Reduce muxing delay for lower latency
        .arg("-max_muxing_queue_size")
        .arg("1024") // Limit muxing queue to prevent memory issues
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
                                    if let Some((compact, speed)) =
                                        extract_compact_stats(&line_buffer)
                                    {
                                        print!("\r{}", compact);
                                        let _ = std::io::stdout().flush();
                                        // Update global speed if available (lock-free atomic write)
                                        if let Some(s) = speed {
                                            FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
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
                                        if let Some((compact, speed)) =
                                            extract_compact_stats(&line_buffer)
                                        {
                                            println!("\r{}", compact);
                                            // Update global speed if available (lock-free atomic write)
                                            if let Some(s) = speed {
                                                FFMPEG_SPEED.store(s.to_bits(), Ordering::Relaxed);
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
pub async fn wait_ffmpeg() -> Option<std::process::ExitStatus> {
    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
    if let Some(mut process) = supervisor.take() {
        match process.wait().await {
            Ok(status) => {
                if let Some(code) = status.code() {
                    tracing::info!("ffmpeg exited with status code: {}", code);
                } else {
                    tracing::info!("ffmpeg terminated by signal");
                }
                Some(status)
            }
            Err(e) => {
                tracing::error!("Failed to wait for ffmpeg: {}", e);
                None
            }
        }
    } else {
        None
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
            stop_ffmpeg().await;
            break;
        }

        // Check every 5 seconds
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}
