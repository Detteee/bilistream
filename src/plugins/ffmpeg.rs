use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
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
    static ref FFMPEG_CACHE_SPEED: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_BITRATE_KBPS: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_CACHE_BITRATE_KBPS: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_TOTAL_BYTES: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_CACHE_TOTAL_BYTES: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_LAST_SAMPLE_MS: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_CACHE_LAST_SAMPLE_MS: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_HLS_CACHE_ACTIVE: AtomicBool = AtomicBool::new(false);
    // Track last progress time for timeout detection (stored as Unix timestamp in seconds)
    static ref LAST_PROGRESS_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track last reported stream time from ffmpeg (stored as seconds, converted from HH:MM:SS.ms)
    static ref LAST_STREAM_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when stream time last changed (Unix timestamp in seconds)
    static ref LAST_STREAM_TIME_UPDATE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when speed first dropped below LOW_SPEED_THRESHOLD (0 = speed is OK)
    static ref LOW_SPEED_SINCE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_STATS_DISPLAY: Arc<StdMutex<FfmpegStatsDisplay>> =
        Arc::new(StdMutex::new(FfmpegStatsDisplay::default()));
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
                tracing::warn!("⚠️ 设置进程优先级失败: {}", stderr.trim());
                tracing::info!("💡 提示: 使用 sudo 运行，或设置 CAP_SYS_NICE 能力以获得更好性能");
            }
            Err(e) => {
                tracing::warn!("⚠️ 无法设置进程优先级: {}", e);
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
                tracing::warn!("⚠️ 设置进程优先级失败: {}", stderr.trim());
            }
            Err(e) => {
                tracing::warn!("⚠️ 无法设置进程优先级: {}", e);
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
                tracing::warn!("⚠️ 设置进程优先级失败: {}", stderr.trim());
                tracing::info!("💡 提示: 使用 sudo 运行以获得更好性能");
            }
            Err(e) => {
                tracing::warn!("⚠️ 无法设置进程优先级: {}", e);
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

// Get current ffmpeg push speed (lock-free read)
pub async fn get_ffmpeg_speed() -> Option<f32> {
    let bits = FFMPEG_SPEED.load(Ordering::Relaxed);
    if bits == 0 {
        None
    } else {
        Some(f32::from_bits(bits))
    }
}

// Get current ffmpeg HLS cache writer speed (lock-free read)
pub async fn get_ffmpeg_cache_speed() -> Option<f32> {
    let bits = FFMPEG_CACHE_SPEED.load(Ordering::Relaxed);
    if bits == 0 {
        None
    } else {
        Some(f32::from_bits(bits))
    }
}

pub async fn is_ffmpeg_hls_cache_active() -> bool {
    FFMPEG_HLS_CACHE_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FfmpegNetworkStats {
    pub push_bitrate_kbps: Option<f32>,
    pub cache_bitrate_kbps: Option<f32>,
    pub push_total_bytes: u64,
    pub cache_total_bytes: u64,
}

pub async fn get_ffmpeg_network_stats() -> FfmpegNetworkStats {
    FfmpegNetworkStats {
        push_bitrate_kbps: f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS),
        cache_bitrate_kbps: f32_from_atomic_bits(&FFMPEG_CACHE_BITRATE_KBPS),
        push_total_bytes: FFMPEG_TOTAL_BYTES.load(Ordering::Relaxed),
        cache_total_bytes: FFMPEG_CACHE_TOTAL_BYTES.load(Ordering::Relaxed),
    }
}

fn f32_from_atomic_bits(value: &AtomicU32) -> Option<f32> {
    let bits = value.load(Ordering::Relaxed);
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
            tracing::info!("🛑 正在停止 ffmpeg 进程组 (主 PID: {})", pid_value);
        }
        let cache_dir = process.cache_dir.clone();

        // Try tokio kill first
        match process.kill().await {
            Ok(_) => {
                // Successfully killed via tokio
            }
            Err(e) => {
                tracing::warn!("⚠️ Tokio 终止失败: {}，尝试系统 kill", e);

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
                                tracing::error!("❌ 系统 kill 失败: {}", stderr);
                            }
                            Err(e) => {
                                tracing::error!("❌ 执行 kill 命令失败: {}", e);
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
                                tracing::error!("❌ taskkill 失败: {}", stderr);
                            }
                            Err(e) => {
                                tracing::error!("❌ 执行 taskkill 失败: {}", e);
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
                tracing::warn!("⚠️ 删除 HLS 缓存目录失败 {}: {}", cache_dir.display(), e);
            }
        }
        tracing::info!("✅ ffmpeg 进程已停止");
    } else {
        tracing::info!("没有需要停止的 ffmpeg 进程");
    }

    // Clear speed, progress time, and rendered stats when ffmpeg stops.
    reset_stats_display();
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_HLS_CACHE_ACTIVE.store(false, Ordering::Relaxed);
    LAST_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
}
const NETWORK_PANEL_WIDTH: usize = 72;
const NETWORK_PANEL_CONTENT_WIDTH: usize = NETWORK_PANEL_WIDTH - 4;
const NETWORK_GRAPH_WIDTH: usize = NETWORK_PANEL_CONTENT_WIDTH - 11;
const NETWORK_HISTORY_LIMIT: usize = 60;
const NETWORK_SAMPLE_GAP_LIMIT_MS: u64 = 10_000;

#[derive(Clone, Copy)]
enum FfmpegStatsRole {
    Cache,
    Push,
}

#[derive(Clone, Default)]
struct FfmpegStatsSample {
    time: Option<String>,
    bitrate: Option<String>,
    bitrate_kbps: Option<f32>,
    speed: Option<f32>,
    stream_time_secs: Option<u32>,
}

#[derive(Default)]
struct FfmpegStatsDisplay {
    cache: Option<FfmpegStatsSample>,
    push: Option<FfmpegStatsSample>,
    cache_history: Vec<f32>,
    push_history: Vec<f32>,
    rendered_lines: usize,
    enabled: bool,
}

impl FfmpegStatsDisplay {
    fn reset(&mut self) {
        if self.rendered_lines > 0 && self.enabled {
            eprint!("\x1b[{}F\x1b[J", self.rendered_lines);
            let _ = std::io::stderr().flush();
        }
        *self = Self {
            enabled: std::io::stderr().is_terminal(),
            ..Self::default()
        };
    }

    fn update(&mut self, role: FfmpegStatsRole, sample: FfmpegStatsSample) {
        let rate = sample.bitrate_kbps.unwrap_or(0.0);
        match role {
            FfmpegStatsRole::Cache => {
                push_history_sample(&mut self.cache_history, rate);
                self.cache = Some(sample);
            }
            FfmpegStatsRole::Push => {
                push_history_sample(&mut self.push_history, rate);
                self.push = Some(sample);
            }
        }
        self.render();
    }

    fn render(&mut self) {
        if !self.enabled {
            return;
        }

        let scale_kbps = self
            .cache_history
            .iter()
            .chain(self.push_history.iter())
            .copied()
            .fold(0.0_f32, f32::max)
            .max(1.0);

        let lines = [
            network_top_border(),
            network_content_line(&format!(
                "Auto scale {:>12}",
                format_network_rate(scale_kbps)
            )),
            network_content_line(&Self::meter_row(
                "Cache RX",
                self.cache.as_ref(),
                FFMPEG_CACHE_TOTAL_BYTES.load(Ordering::Relaxed),
            )),
            network_content_line(&Self::graph_row("Cache", &self.cache_history, scale_kbps)),
            network_content_line(&Self::meter_row(
                "RTMP TX",
                self.push.as_ref(),
                FFMPEG_TOTAL_BYTES.load(Ordering::Relaxed),
            )),
            network_content_line(&Self::graph_row("Push", &self.push_history, scale_kbps)),
            network_bottom_border(),
        ];

        if self.rendered_lines > 0 {
            eprint!("\x1b[{}F", self.rendered_lines);
        }
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                eprintln!("\x1b[2K\x1b[1m{}", line);
            } else if index == lines.len() - 1 {
                eprintln!("\x1b[2K{}\x1b[0m", line);
            } else {
                eprintln!("\x1b[2K{}", line);
            }
        }
        let _ = std::io::stderr().flush();
        self.rendered_lines = lines.len();
    }

    fn meter_row(label: &str, sample: Option<&FfmpegStatsSample>, total_bytes: u64) -> String {
        let bitrate = sample
            .and_then(|s| s.bitrate_kbps)
            .map(format_network_rate)
            .unwrap_or_else(|| "-".to_string());
        let speed = sample
            .and_then(|s| s.speed.map(|v| format!("{:.2}x", v)))
            .unwrap_or_else(|| "-".to_string());
        truncate_cell(
            &format!(
                "{:<8} {:>12}  {:>6}  Total {:>10}",
                label,
                bitrate,
                speed,
                format_bytes(total_bytes)
            ),
            NETWORK_PANEL_CONTENT_WIDTH,
        )
    }

    fn graph_row(label: &str, history: &[f32], scale_kbps: f32) -> String {
        format!(
            "{:<8} {}",
            label,
            sparkline(history, NETWORK_GRAPH_WIDTH, scale_kbps)
        )
    }
}

fn network_top_border() -> String {
    let title = "─ Network ";
    format!(
        "┌{}{}┐",
        title,
        "─".repeat(NETWORK_PANEL_WIDTH - 2 - title.chars().count())
    )
}

fn network_bottom_border() -> String {
    format!("└{}┘", "─".repeat(NETWORK_PANEL_WIDTH - 2))
}

fn network_content_line(value: &str) -> String {
    format!("│ {} │", truncate_cell(value, NETWORK_PANEL_CONTENT_WIDTH))
}

fn push_history_sample(history: &mut Vec<f32>, value: f32) {
    history.push(value.max(0.0));
    if history.len() > NETWORK_HISTORY_LIMIT {
        history.remove(0);
    }
}

fn sparkline(history: &[f32], width: usize, scale_kbps: f32) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if history.is_empty() {
        return " ".repeat(width);
    }

    let start = history.len().saturating_sub(width);
    let mut output = String::with_capacity(width);
    for _ in 0..width.saturating_sub(history.len() - start) {
        output.push(' ');
    }
    for value in &history[start..] {
        let ratio = if scale_kbps > 0.0 {
            (value / scale_kbps).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let index = (ratio * (BARS.len() - 1) as f32).round() as usize;
        output.push(BARS[index]);
    }
    output
}

fn format_network_rate(kbps: f32) -> String {
    if kbps >= 1000.0 {
        format!("{:.2} Mb/s", kbps / 1000.0)
    } else {
        format!("{:.0} Kb/s", kbps)
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

fn truncate_cell(value: &str, width: usize) -> String {
    let mut text: String = value.chars().take(width).collect();
    while text.chars().count() < width {
        text.push(' ');
    }
    text
}

fn reset_stats_display() {
    if let Ok(mut display) = FFMPEG_STATS_DISPLAY.lock() {
        display.reset();
    }
}

fn update_stats_display(role: FfmpegStatsRole, sample: FfmpegStatsSample) {
    if let Ok(mut display) = FFMPEG_STATS_DISPLAY.lock() {
        display.update(role, sample);
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn update_network_counters(role: FfmpegStatsRole, bitrate_kbps: f32) {
    let now = now_millis();
    let (bitrate, total, last_sample) = match role {
        FfmpegStatsRole::Cache => (
            &*FFMPEG_CACHE_BITRATE_KBPS,
            &*FFMPEG_CACHE_TOTAL_BYTES,
            &*FFMPEG_CACHE_LAST_SAMPLE_MS,
        ),
        FfmpegStatsRole::Push => (
            &*FFMPEG_BITRATE_KBPS,
            &*FFMPEG_TOTAL_BYTES,
            &*FFMPEG_LAST_SAMPLE_MS,
        ),
    };

    bitrate.store(bitrate_kbps.to_bits(), Ordering::Relaxed);
    let previous = last_sample.swap(now, Ordering::Relaxed);
    if previous == 0 {
        return;
    }

    let elapsed_ms = now.saturating_sub(previous);
    if elapsed_ms == 0 || elapsed_ms > NETWORK_SAMPLE_GAP_LIMIT_MS {
        return;
    }

    let bytes = (bitrate_kbps as f64 * 1000.0 / 8.0 * elapsed_ms as f64 / 1000.0).round();
    if bytes.is_finite() && bytes > 0.0 {
        total.fetch_add(bytes as u64, Ordering::Relaxed);
    }
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

fn parse_bitrate_to_kbps(value: &str) -> Option<f32> {
    let trimmed = value.trim();
    if trimmed == "N/A" {
        return None;
    }

    let number: String = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let parsed = number.parse::<f32>().ok()?;
    let unit = trimmed[number.len()..].trim().to_ascii_lowercase();

    if unit.starts_with("mbit") {
        Some(parsed * 1000.0)
    } else if unit.starts_with("bit") {
        Some(parsed / 1000.0)
    } else {
        Some(parsed)
    }
}

fn value_after_key<'a>(parts: &[&'a str], idx: usize, key: &str) -> Option<&'a str> {
    let value = parts.get(idx)?.strip_prefix(key)?;
    if value.is_empty() {
        parts.get(idx + 1).copied()
    } else {
        Some(value)
    }
}

fn extract_ffmpeg_stats(line: &str) -> Option<FfmpegStatsSample> {
    if !line.starts_with("frame=") && !line.contains("fps=") {
        return None;
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    let mut sample = FfmpegStatsSample::default();

    for (idx, _) in parts.iter().enumerate() {
        if let Some(value) = value_after_key(&parts, idx, "time=") {
            sample.time = Some(value.to_string());
            sample.stream_time_secs = parse_time_to_seconds(value);
        } else if let Some(value) = value_after_key(&parts, idx, "bitrate=") {
            sample.bitrate = Some(value.to_string());
            sample.bitrate_kbps = parse_bitrate_to_kbps(value);
        } else if let Some(value) = value_after_key(&parts, idx, "speed=") {
            if let Ok(parsed) = value.trim_end_matches('x').parse::<f32>() {
                sample.speed = Some(parsed);
            }
        }
    }

    sample.time.as_ref()?;
    Some(sample)
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
    stats_role: Option<FfmpegStatsRole>,
) {
    tokio::spawn(async move {
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
                    handle_ffmpeg_stderr_line(
                        &line_buffer,
                        log_level.as_str(),
                        process_name,
                        stats_role,
                    );
                    line_buffer.clear();
                } else if ch == '\n' {
                    handle_ffmpeg_stderr_line(
                        &line_buffer,
                        log_level.as_str(),
                        process_name,
                        stats_role,
                    );
                    line_buffer.clear();
                } else {
                    line_buffer.push(ch);
                }
            }
        }
    });
}

fn handle_ffmpeg_stderr_line(
    line: &str,
    log_level: &str,
    process_name: &'static str,
    stats_role: Option<FfmpegStatsRole>,
) {
    if line.is_empty() {
        return;
    }

    if let Some(sample) = extract_ffmpeg_stats(line) {
        update_progress_time();
        if let Some(role) = stats_role {
            if let Some(bitrate_kbps) = sample.bitrate_kbps {
                update_network_counters(role, bitrate_kbps);
            }
            match role {
                FfmpegStatsRole::Cache => {
                    if let Some(speed) = sample.speed {
                        FFMPEG_CACHE_SPEED.store(speed.to_bits(), Ordering::Relaxed);
                    }
                }
                FfmpegStatsRole::Push => {
                    if let Some(speed) = sample.speed {
                        FFMPEG_SPEED.store(speed.to_bits(), Ordering::Relaxed);
                        update_speed_tracking(speed);
                    }
                    if let Some(stream_time) = sample.stream_time_secs {
                        update_stream_time(stream_time);
                    }
                }
            }
            update_stats_display(role, sample);
        }
    } else if line.contains("error") || line.contains("Error") {
        tracing::error!("{}: {}", process_name, line);
    } else if is_harmless_hls_keepalive_warning(line) {
        tracing::debug!("{}: {}", process_name, line);
    } else if line.contains("warning") || line.contains("Warning") {
        tracing::warn!("{}: {}", process_name, line);
    } else if log_level == "debug" || log_level == "info" {
        tracing::debug!("{}: {}", process_name, line);
    }
}

/// Input options for remote live HLS (YouTube/Twitch CDN).
///
/// Segment URLs often rotate across different hosts; reusing HTTP connections
/// then triggers "keepalive request failed" / "Cannot reuse HTTP connection"
/// warnings (see https://github.com/mpv-player/mpv/issues/8500).
fn append_remote_hls_input_options(cmd: &mut Command) {
    cmd.arg("-http_persistent")
        .arg("0")
        .arg("-thread_queue_size")
        .arg("2048");
}

fn is_harmless_hls_keepalive_warning(line: &str) -> bool {
    line.contains("keepalive request failed")
        || line.contains("Cannot reuse HTTP connection for different host")
}

fn append_crop_or_copy(cmd: &mut Command, crop: Option<(u32, u32, u32, u32)>) {
    if let Some((width, height, x, y)) = crop {
        tracing::info!("🎬 应用裁剪滤镜: {}:{}:{}:{}", width, height, x, y);
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
    reset_stats_display();
    update_progress_time();
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
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
    tracing::info!("⏱️ HLS 缓存已关闭: 直接转播");

    let mut cmd = Command::new(&ffmpeg_cmd);
    #[cfg(target_os = "windows")]
    configure_no_window(&mut cmd);

    if let Some(proxy_url) = proxy {
        cmd.arg("-http_proxy").arg(proxy_url);
    }

    cmd.arg("-nostdin")
        .arg("-stats")
        .arg("-loglevel")
        .arg(&log_level);
    append_remote_hls_input_options(&mut cmd);
    cmd.arg("-re")
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
            tracing::info!("🚀 ffmpeg 进程已启动 (PID: {:?})", pid);

            if let Some(pid_value) = pid {
                set_high_priority(pid_value);
            }

            reset_ffmpeg_tracking_state();
            FFMPEG_HLS_CACHE_ACTIVE.store(false, Ordering::Relaxed);

            if let Some(stderr) = child.stderr.take() {
                spawn_ffmpeg_stderr_monitor(
                    stderr,
                    log_level,
                    "ffmpeg",
                    Some(FfmpegStatsRole::Push),
                );
            }

            let process = FfmpegProcess {
                children: vec![child],
                pid,
                cache_dir: None,
            };
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            *supervisor = Some(process);

            start_ffmpeg_timeout_monitor(STARTUP_NO_STATS_TIMEOUT_SECS);
        }
        Err(e) => {
            tracing::error!("❌ 启动 ffmpeg 失败: {}", e);
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
            tracing::error!("❌ 创建 HLS 缓存目录失败: {}", e);
            return;
        }
    };
    let playlist_path = cache_dir.join("index.m3u8");
    let segment_pattern = cache_dir.join("segment_%06d.ts");
    let ffmpeg_cmd = get_ffmpeg_command();

    tracing::info!(
        "⏱️ HLS 缓存已启用: {} 秒输入到输出延迟 ({})",
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
        .arg("warning");
    append_remote_hls_input_options(&mut cache_cmd);
    cache_cmd
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
            tracing::info!("🚀 ffmpeg HLS 缓存写入进程已启动 (PID: {:?})", cache_pid);

            if let Some(pid_value) = cache_pid {
                set_high_priority(pid_value);
            }

            reset_ffmpeg_tracking_state();
            FFMPEG_HLS_CACHE_ACTIVE.store(true, Ordering::Relaxed);

            if let Some(stderr) = cache_child.stderr.take() {
                spawn_ffmpeg_stderr_monitor(
                    stderr,
                    log_level.clone(),
                    "ffmpeg 缓存写入",
                    Some(FfmpegStatsRole::Cache),
                );
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
                        "❌ 等待 {} 秒后仍未创建 HLS 缓存播放列表: {}",
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
                        tracing::info!("🚀 ffmpeg 延迟推流进程已启动 (PID: {:?})", reader_pid);
                        if let Some(pid_value) = reader_pid {
                            set_high_priority(pid_value);
                        }
                        if let Some(stderr) = reader_child.stderr.take() {
                            spawn_ffmpeg_stderr_monitor(
                                stderr,
                                reader_log_level,
                                "ffmpeg 延迟推流",
                                Some(FfmpegStatsRole::Push),
                            );
                        }

                        let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
                        if let Some(process) = supervisor.as_mut() {
                            process.children.push(reader_child);
                        }
                    }
                    Err(e) => {
                        tracing::error!("❌ 启动延迟 RTMP ffmpeg 失败: {}", e);
                        stop_ffmpeg_internal(false).await;
                    }
                }
            });

            start_ffmpeg_timeout_monitor(cache_startup_timeout_secs(latency_secs));
        }
        Err(e) => {
            tracing::error!("❌ 启动 ffmpeg 缓存写入进程失败: {}", e);
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
                            tracing::info!("ffmpeg 已退出，状态码: {}", code);
                        } else {
                            tracing::info!("ffmpeg 已被信号终止");
                        }

                        drop(supervisor);
                        stop_ffmpeg_internal(false).await;
                        return Some(status);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("检查 ffmpeg 状态失败: {}", e);
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
                        "⚠️ ffmpeg 似乎卡住（{} 秒无 stats 输出），正在终止进程",
                        elapsed_secs
                    );
                }
                StuckReason::StreamTimeFrozen { elapsed_secs } => {
                    tracing::error!(
                        "⚠️ ffmpeg 似乎卡住（流时间冻结 {} 秒），正在终止进程",
                        elapsed_secs
                    );
                }
                StuckReason::LowSpeed { elapsed_secs } => {
                    tracing::error!(
                        "⚠️ ffmpeg 似乎卡住（速度低于 {} 持续 {} 秒），正在终止进程",
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
