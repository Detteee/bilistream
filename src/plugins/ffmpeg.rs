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

/// Stops the supervised ffmpeg process
pub async fn stop_ffmpeg() {
    tracing::info!("🛑 Stopping ffmpeg process...");

    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
    if let Some(mut process) = supervisor.take() {
        if let Some(pid) = process.pid() {
            tracing::info!("Terminating ffmpeg process (PID: {})", pid);
        }

        // Try graceful shutdown first
        if let Err(e) = process.kill().await {
            tracing::error!("❌ Failed to kill ffmpeg: {}", e);
        } else {
            tracing::info!("✅ ffmpeg process stopped successfully");
        }
    } else {
        tracing::warn!("⚠️ No ffmpeg process to stop");
    }

    // Clear speed when ffmpeg stops (lock-free write)
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
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

    if let Some(proxy) = proxy {
        cmd.arg("-http_proxy").arg(proxy);
    }

    // Input options
    cmd.arg("-re") // Read input at native frame rate
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
        // RTMP settings
        .arg("-rtmp_buffer")
        .arg("10240k")
        .arg("-rtmp_live")
        .arg("live")
        .arg("-f")
        .arg("flv")
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
