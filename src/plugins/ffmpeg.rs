use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex, MutexGuard};
use tokio::task::JoinHandle;

use super::utils::{configure_tokio_no_window, executable_command, set_high_priority};
use crate::logging::{
    exit_status, has_sensitive_data, human_duration, safe_diagnostic, safe_diagnostic_continuation,
    safe_text,
};

// Stuck-detection thresholds for live HLS restreaming.
const LOW_SPEED_THRESHOLD: f32 = 0.98;
const LOW_SPEED_TIMEOUT_SECS: u32 = 60;
const STREAM_TIME_FROZEN_SECS: u32 = 10;
const STARTUP_NO_STATS_TIMEOUT_SECS: u64 = 30;
const CACHE_PLAYLIST_WAIT_SECS: u64 = 30;
// When a source ends, ffmpeg may keep printing stats with a slowly decaying speed
// while TX/RX stay at zero. Treat sustained zero transfer as drain-complete.
const NETWORK_IDLE_BITRATE_KBPS: f32 = 1.0;
// Match STREAM_TIME_FROZEN_SECS: pre-bfcccde HLS cache mode used stream-time
// freeze for fast end-of-stream shutdown; bfcccde disabled it entirely.
const IDLE_DRAIN_TIMEOUT_SECS: u32 = STREAM_TIME_FROZEN_SECS;

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

/// Pre-muxed MPEG-TS producer (streamlink nicolive) piped into ffmpeg.
#[derive(Clone)]
pub struct PipedIngest {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone)]
pub enum FfmpegSource {
    HlsUrl(String),
    Piped(PipedIngest),
}

fn cache_startup_timeout_secs(latency_secs: u64) -> u64 {
    latency_secs + CACHE_PLAYLIST_WAIT_SECS
}

#[derive(Debug)]
enum StuckReason {
    NoStats {
        elapsed_secs: u32,
        source: &'static str,
    },
    StreamTimeFrozen {
        elapsed_secs: u32,
    },
    LowSpeed {
        elapsed_secs: u32,
    },
    IdleDrain {
        elapsed_secs: u32,
    },
}

// Global process supervisor
lazy_static::lazy_static! {
    static ref FFMPEG_SUPERVISOR: Arc<ProcessSupervisor> = Arc::new(ProcessSupervisor::default());
    // Use atomic for lock-free speed updates (stored as f32 bits)
    static ref FFMPEG_SPEED: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_CACHE_SPEED: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_BITRATE_KBPS: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_CACHE_BITRATE_KBPS: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_PUSH_FPS: Arc<AtomicU32> =
        Arc::new(AtomicU32::new(OPTIONAL_F32_NONE_BITS));
    static ref FFMPEG_TOTAL_BYTES: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_CACHE_TOTAL_BYTES: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_PUSH_FRAME: Arc<AtomicU64> = Arc::new(AtomicU64::new(OPTIONAL_U64_NONE));
    static ref FFMPEG_LAST_SAMPLE_MS: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_CACHE_LAST_SAMPLE_MS: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    static ref FFMPEG_HLS_CACHE_ACTIVE: AtomicBool = AtomicBool::new(false);
    // Track last progress time for timeout detection (stored as Unix timestamp in seconds)
    static ref LAST_PROGRESS_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref LAST_PUSH_PROGRESS_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref LAST_CACHE_PROGRESS_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_PUSH_ACTIVE: AtomicBool = AtomicBool::new(false);
    static ref FFMPEG_PUSH_STARTED_AT: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track last reported stream time from ffmpeg (stored as seconds, converted from HH:MM:SS.ms)
    static ref LAST_STREAM_TIME: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_CACHE_TIME_SECS: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when stream time last changed (Unix timestamp in seconds)
    static ref LAST_STREAM_TIME_UPDATE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when speed first dropped below LOW_SPEED_THRESHOLD (0 = speed is OK)
    static ref LOW_SPEED_SINCE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    // Track when network transfer dropped to idle (0 = transfer active)
    static ref NETWORK_IDLE_SINCE: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    static ref FFMPEG_STATS_DISPLAY: Arc<StdMutex<FfmpegStatsDisplay>> =
        Arc::new(StdMutex::new(FfmpegStatsDisplay::default()));
}

// Track if ffmpeg was stopped manually (e.g., via restart button)
static MANUAL_STOP: AtomicBool = AtomicBool::new(false);

// Track if a manual restart was requested (force immediate restart even if stream is live)
static MANUAL_RESTART: AtomicBool = AtomicBool::new(false);

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FfmpegSession(u64);

impl fmt::Display for FfmpegSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Immutable, non-secret identity captured from the selected stream, never
/// looked up from the next configuration by a delayed child or log writer.
#[derive(Clone)]
pub struct FfmpegStreamIdentity {
    node: String,
    platform: String,
    channel: String,
    channel_id: String,
}

impl FfmpegStreamIdentity {
    pub fn new(node: &str, platform: &str, channel: &str, channel_id: &str) -> Self {
        Self {
            node: safe_text(
                if node.trim().is_empty() {
                    "local"
                } else {
                    node
                },
                usize::MAX,
            ),
            platform: safe_text(platform, usize::MAX),
            channel: safe_text(
                if channel.is_empty() {
                    channel_id
                } else {
                    channel
                },
                24,
            ),
            channel_id: safe_text(channel_id, usize::MAX),
        }
    }

    pub(crate) fn session_label(&self, session: Option<FfmpegSession>) -> String {
        match session {
            Some(session) => format!("{self} s={session}"),
            None => format!("{self} 未建立会话"),
        }
    }
}

impl fmt::Display for FfmpegStreamIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}/{}", self.node, self.platform, self.channel)
    }
}

#[derive(Clone)]
struct SessionLogContext {
    identity: Arc<FfmpegStreamIdentity>,
    session: FfmpegSession,
}

impl SessionLogContext {
    fn new(identity: FfmpegStreamIdentity, session: FfmpegSession) -> Self {
        Self {
            identity: Arc::new(identity),
            session,
        }
    }
}

impl fmt::Display for SessionLogContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} s={}", self.identity, self.session)
    }
}

#[derive(Clone, Copy)]
enum ChildRole {
    Ingest,
    Ffmpeg(FfmpegStatsRole),
}

impl ChildRole {
    fn stats_role(self) -> Option<FfmpegStatsRole> {
        match self {
            Self::Ingest => None,
            Self::Ffmpeg(role) => Some(role),
        }
    }
}

impl fmt::Display for ChildRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ingest => "摄取",
            Self::Ffmpeg(FfmpegStatsRole::Cache) => "缓存",
            Self::Ffmpeg(FfmpegStatsRole::Push) => "推流",
        })
    }
}

#[derive(Clone)]
struct ChildLogContext {
    session: SessionLogContext,
    role: ChildRole,
    pid: Option<u32>,
    started: Instant,
}

impl ChildLogContext {
    fn new(session: &SessionLogContext, role: ChildRole, pid: Option<u32>) -> Self {
        Self {
            session: session.clone(),
            role,
            pid,
            started: Instant::now(),
        }
    }

    fn pid_label(&self) -> String {
        self.pid
            .map_or_else(|| "未知".to_string(), |pid| pid.to_string())
    }

    fn started(&self) {
        let state = if matches!(self.role, ChildRole::Ffmpeg(FfmpegStatsRole::Cache)) {
            "；正在预缓冲，等待推流进程"
        } else {
            ""
        };
        tracing::info!(
            "{self} 进程已启动 pid={}{state}；channel={}",
            self.pid_label(),
            self.session.identity.channel_id
        );
    }

    fn exited(&self, status: &std::process::ExitStatus) {
        let elapsed = human_duration(self.started.elapsed().as_secs() as i64);
        let description = exit_status(status);
        if status.success() {
            tracing::info!(
                "{self} 进程已退出 pid={} {description}；运行{elapsed}",
                self.pid_label()
            );
        } else {
            tracing::warn!(
                "{self} 进程异常退出 pid={} {description}；运行{elapsed}，等待复核直播状态",
                self.pid_label()
            );
        }
    }
}

impl fmt::Display for ChildLogContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.session, self.role)
    }
}

struct FfmpegChild {
    child: Child,
    log: ChildLogContext,
}

#[derive(Default)]
struct SessionTasks(Vec<JoinHandle<()>>);

impl Drop for SessionTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Long preparation and cleanup futures may own the process lock. A fence
/// cancels those futures before acquiring the lock, without borrowing PIDs.
struct ProcessSupervisor {
    process: Mutex<Option<FfmpegProcess>>,
    fencing: watch::Sender<bool>,
    fence_lock: Mutex<()>,
}

impl Default for ProcessSupervisor {
    fn default() -> Self {
        Self {
            process: Mutex::new(None),
            fencing: watch::channel(false).0,
            fence_lock: Mutex::new(()),
        }
    }
}

impl ProcessSupervisor {
    async fn lock(&self) -> MutexGuard<'_, Option<FfmpegProcess>> {
        self.process.lock().await
    }

    async fn interruptible<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        let mut fence = self.fencing.subscribe();
        if *fence.borrow_and_update() {
            return None;
        }
        tokio::select! {
            biased;
            _ = fence.changed() => None,
            result = work => Some(result),
        }
    }

    fn spawn(&self, command: &mut Command) -> std::io::Result<Child> {
        // Serialize the spawn syscall with fence admission. A child spawned
        // just before admission is owned by its interruptible future or the
        // supervisor; either path retains kill_on_drop through cancellation.
        let fencing = self.fencing.borrow();
        if *fencing {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "推流已隔离",
            ));
        }
        command.spawn()
    }

    async fn stop_matching(
        &self,
        expected: Option<FfmpegSession>,
        manual: bool,
        cleanup: impl Future<Output = ()>,
    ) -> bool {
        self.interruptible(async {
            let mut supervisor = self.lock().await;
            if expected.is_some_and(|session| matching_process(&mut supervisor, session).is_none())
            {
                return false;
            }
            if manual {
                MANUAL_STOP.store(true, Ordering::SeqCst);
            }
            stop_locked(&mut supervisor, manual.then_some("主动停止请求")).await;
            cleanup.await;
            true
        })
        .await
        .unwrap_or(false)
    }

    async fn transition(
        &self,
        session: FfmpegSession,
        prepare: impl Future<Output = bool>,
    ) -> bool {
        self.interruptible(async {
            let mut supervisor = self.lock().await;
            if matching_process(&mut supervisor, session).is_none() || !prepare.await {
                return false;
            }
            stop_locked(&mut supervisor, Some("切换准备完成")).await;
            true
        })
        .await
        .unwrap_or(false)
    }

    async fn fence(&self) {
        let _exclusive = self.fence_lock.lock().await;
        self.fencing.send_replace(true);
        let mut supervisor = self.lock().await;
        let mut kill_failures = Vec::new();
        let mut fenced = None;
        if let Some(mut process) = supervisor.take() {
            // Send every publishing child its kill signal before any ingest,
            // process-reaping or filesystem work can delay another publisher.
            for child in &mut process.children {
                if let Err(error) = child.child.start_kill() {
                    kill_failures.push((child.log.clone(), error));
                }
            }
            drop(std::mem::take(&mut process.tasks));
            let cache_dir = process.cache_dir.take();
            fenced = Some((process.log.clone(), process.roles_label()));
            // Owned children kill on drop; IngestProcess also kills its group.
            drop(process);
            if let Some(cache_dir) = cache_dir {
                tokio::spawn(async move {
                    let _ = tokio::fs::remove_dir_all(cache_dir).await;
                });
            }
        }
        reset_stopped_tracking_state();
        // Startup still rechecks the runtime's execution fence under this lock.
        self.fencing.send_replace(false);
        drop(supervisor);
        drop(_exclusive);

        // A slow writer must not delay another kill, ingest-group drop, or the
        // release of either supervisor lock. These snapshots retain ownership.
        for (log, error) in kill_failures {
            tracing::warn!(
                "{log} 隔离时发送终止信号失败 pid={}；{}",
                log.pid_label(),
                safe_diagnostic(&error.to_string())
            );
        }
        if let Some((context, roles)) = fenced {
            tracing::info!("{context} 会话执行已隔离，已请求终止子进程；角色={roles}");
        }
    }
}

// Represents a managed ffmpeg process
pub struct FfmpegProcess {
    session: FfmpegSession,
    log: SessionLogContext,
    tasks: SessionTasks,
    children: Vec<FfmpegChild>,
    ingest: Option<IngestProcess>,
    pid: Option<u32>,
    cache_dir: Option<PathBuf>,
}

impl FfmpegProcess {
    fn roles_label(&self) -> String {
        self.ingest
            .iter()
            .map(|ingest| ingest.1.role)
            .chain(self.children.iter().map(|child| child.log.role))
            .map(|role| role.to_string())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(child) = self.children.first_mut() {
            child.child.wait().await
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no ffmpeg child process",
            ))
        }
    }

    pub async fn kill(&mut self) -> std::io::Result<()> {
        let mut result = Ok(());
        if let Some(ingest) = self.ingest.as_mut() {
            kill_child_tree(ingest).await;
        }
        for child in &mut self.children {
            if let Err(e) = child.child.kill().await {
                result = Err(std::io::Error::new(
                    e.kind(),
                    format!(
                        "{} pid={}；{}",
                        child.log.role,
                        child.log.pid_label(),
                        safe_diagnostic(&e.to_string())
                    ),
                ));
            }
        }
        result
    }
}

fn matching_process(
    supervisor: &mut Option<FfmpegProcess>,
    session: FfmpegSession,
) -> Option<&mut FfmpegProcess> {
    supervisor
        .as_mut()
        .filter(|process| process.session == session)
}

pub async fn current_ffmpeg_session() -> Option<FfmpegSession> {
    FFMPEG_SUPERVISOR
        .lock()
        .await
        .as_ref()
        .map(|process| process.session)
}

/// Register work while checking ownership under the supervisor lock.
pub async fn spawn_session_task(
    session: FfmpegSession,
    task: impl Future<Output = ()> + Send + 'static,
) {
    let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
    if let Some(process) = matching_process(&mut supervisor, session) {
        process.tasks.0.push(tokio::spawn(task));
    }
}

// Own the process group as well as the direct child: cancellation during startup
// must also terminate streamlink's muxer.
struct IngestProcess(Child, ChildLogContext);

impl Drop for IngestProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.id() {
            // SAFETY: spawn_piped_ingest creates a separate process group whose
            // ID is the unreaped child's PID. No borrowed memory crosses FFI.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

async fn kill_child_tree(child: &mut IngestProcess) {
    if let Some(pid) = child.0.id() {
        #[cfg(unix)]
        {
            // SAFETY: this child is the leader of its own process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
        }
    }
    let kill_result = child.0.kill().await;
    let wait_result = child.0.wait().await;
    if let Err(error) = kill_result {
        tracing::warn!(
            "{} 停止摄取失败；{}",
            child.1,
            safe_diagnostic(&error.to_string())
        );
    }
    match wait_result {
        Ok(status) => tracing::debug!(
            "{} 进程已回收 pid={} {}；会话清理",
            child.1,
            child.1.pid_label(),
            exit_status(&status)
        ),
        Err(error) => tracing::warn!(
            "{} 回收摄取进程失败；{}",
            child.1,
            safe_diagnostic(&error.to_string())
        ),
    }
}

fn configure_ingest_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
}

async fn spawn_piped_ingest(
    ingest: &PipedIngest,
    log_level: &str,
    context: &SessionLogContext,
    tasks: &mut SessionTasks,
) -> Option<(IngestProcess, tokio::process::ChildStdout)> {
    let mut cmd = Command::new(&ingest.program);
    configure_tokio_no_window(&mut cmd);
    configure_ingest_process_group(&mut cmd);
    cmd.args(&ingest.args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match FFMPEG_SUPERVISOR.spawn(&mut cmd) {
        Ok(mut child) => {
            let log = ChildLogContext::new(context, ChildRole::Ingest, child.id());
            log.started();
            if let Some(pid) = child.id() {
                set_high_priority(pid);
            }
            if let Some(stderr) = child.stderr.take() {
                tasks.0.push(spawn_ffmpeg_stderr_monitor(
                    stderr,
                    log_level.to_string(),
                    log.clone(),
                ));
            }
            let stdout = child.stdout.take()?;
            Some((IngestProcess(child, log), stdout))
        }
        Err(e) => {
            tracing::error!(
                "{context} 摄取进程启动失败；{}",
                safe_diagnostic(&e.to_string())
            );
            None
        }
    }
}

fn pipe_ingest_stdout(
    cmd: &mut Command,
    stdout: tokio::process::ChildStdout,
    context: &SessionLogContext,
) -> bool {
    match TryInto::<Stdio>::try_into(stdout) {
        Ok(stdio) => {
            cmd.stdin(stdio);
            true
        }
        Err(e) => {
            tracing::error!(
                "{context} 无法连接摄取管道；{}",
                safe_diagnostic(&e.to_string())
            );
            false
        }
    }
}

fn append_source_input(
    cmd: &mut Command,
    source: &FfmpegSource,
    proxy: Option<&str>,
    use_re: bool,
) {
    match source {
        FfmpegSource::HlsUrl(url) => {
            if let Some(proxy_url) = proxy {
                cmd.arg("-http_proxy").arg(proxy_url);
            }
            append_remote_hls_input_options(cmd);
            if use_re {
                cmd.arg("-re");
            }
            cmd.arg("-fflags").arg("+genpts").arg("-i").arg(url);
        }
        FfmpegSource::Piped(_) => {
            cmd.arg("-fflags")
                .arg("+genpts")
                .arg("-f")
                .arg("mpegts")
                .arg("-i")
                .arg("pipe:0");
        }
    }
}

// Helper function to get ffmpeg command path
fn get_ffmpeg_command() -> String {
    executable_command("ffmpeg.exe", "ffmpeg")
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
pub fn get_ffmpeg_speed() -> Option<f32> {
    let bits = FFMPEG_SPEED.load(Ordering::Relaxed);
    if bits == 0 {
        None
    } else {
        Some(f32::from_bits(bits))
    }
}

// Get current ffmpeg HLS cache writer speed (lock-free read)
pub fn get_ffmpeg_cache_speed() -> Option<f32> {
    let bits = FFMPEG_CACHE_SPEED.load(Ordering::Relaxed);
    if bits == 0 {
        None
    } else {
        Some(f32::from_bits(bits))
    }
}

pub fn is_ffmpeg_hls_cache_active() -> bool {
    FFMPEG_HLS_CACHE_ACTIVE.load(Ordering::Relaxed)
}

#[derive(Clone, Debug, Default)]
pub struct FfmpegNetworkStats {
    pub push_bitrate_kbps: Option<f32>,
    pub cache_bitrate_kbps: Option<f32>,
    pub push_fps: Option<f32>,
    pub push_frame: Option<u64>,
    pub push_time_secs: Option<u32>,
    pub cache_time_secs: Option<u32>,
    pub push_bitrate_history: Vec<f32>,
    pub cache_bitrate_history: Vec<f32>,
}

pub fn get_ffmpeg_network_stats() -> FfmpegNetworkStats {
    let (push_bitrate_history, cache_bitrate_history) = snapshot_bitrate_history();
    FfmpegNetworkStats {
        push_bitrate_kbps: f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS),
        cache_bitrate_kbps: f32_from_atomic_bits(&FFMPEG_CACHE_BITRATE_KBPS),
        push_fps: optional_f32_from_atomic_bits(&FFMPEG_PUSH_FPS),
        push_frame: optional_u64_from_atomic(&FFMPEG_PUSH_FRAME),
        push_time_secs: optional_u32_from_atomic(&LAST_STREAM_TIME),
        cache_time_secs: optional_u32_from_atomic(&FFMPEG_CACHE_TIME_SECS),
        push_bitrate_history,
        cache_bitrate_history,
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

fn optional_f32_from_atomic_bits(value: &AtomicU32) -> Option<f32> {
    let bits = value.load(Ordering::Relaxed);
    if bits == OPTIONAL_F32_NONE_BITS {
        return None;
    }

    let value = f32::from_bits(bits);
    value.is_finite().then_some(value)
}

fn optional_u64_from_atomic(value: &AtomicU64) -> Option<u64> {
    let value = value.load(Ordering::Relaxed);
    (value != OPTIONAL_U64_NONE).then_some(value)
}

fn optional_u32_from_atomic(value: &AtomicU32) -> Option<u32> {
    let value = value.load(Ordering::Relaxed);
    (value > 0).then_some(value)
}

fn unix_time_secs_from(time: std::time::SystemTime) -> u32 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

fn unix_time_secs() -> u32 {
    unix_time_secs_from(std::time::SystemTime::now())
}

// Update last stats progress time (lock-free write).
fn update_progress_time(role: Option<FfmpegStatsRole>) {
    let now = unix_time_secs();
    LAST_PROGRESS_TIME.store(now, Ordering::Relaxed);
    match role {
        Some(FfmpegStatsRole::Push) => LAST_PUSH_PROGRESS_TIME.store(now, Ordering::Relaxed),
        Some(FfmpegStatsRole::Cache) => LAST_CACHE_PROGRESS_TIME.store(now, Ordering::Relaxed),
        None => {}
    }
}

fn mark_push_process_started() {
    FFMPEG_PUSH_ACTIVE.store(true, Ordering::Relaxed);
    FFMPEG_PUSH_STARTED_AT.store(unix_time_secs(), Ordering::Relaxed);
}

// Update stream time tracking (lock-free write)
fn update_stream_time(stream_time_secs: u32) {
    let last_time = LAST_STREAM_TIME.load(Ordering::Relaxed);

    // Only update if time has actually progressed
    if stream_time_secs > last_time {
        LAST_STREAM_TIME.store(stream_time_secs, Ordering::Relaxed);

        // Update the timestamp when stream time last changed
        let now = unix_time_secs();
        LAST_STREAM_TIME_UPDATE.store(now, Ordering::Relaxed);
    }
}

fn is_network_transfer_idle(
    hls_cache_active: bool,
    push_active: bool,
    push_bitrate_kbps: f32,
    cache_bitrate_kbps: f32,
) -> bool {
    if hls_cache_active && !push_active {
        return false;
    }

    if hls_cache_active {
        push_bitrate_kbps < NETWORK_IDLE_BITRATE_KBPS
            && cache_bitrate_kbps < NETWORK_IDLE_BITRATE_KBPS
    } else {
        push_bitrate_kbps < NETWORK_IDLE_BITRATE_KBPS
    }
}

fn update_network_idle_tracking(idle: bool) {
    let now = unix_time_secs();
    if idle {
        NETWORK_IDLE_SINCE
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed)
            .ok();
    } else {
        NETWORK_IDLE_SINCE.store(0, Ordering::Relaxed);
    }
}

// Update low-speed tracking: record when speed first drops below threshold, clear when it recovers
fn update_speed_tracking(speed: f32) {
    let now = unix_time_secs();
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
    let now = unix_time_secs();
    let hls_cache_active = FFMPEG_HLS_CACHE_ACTIVE.load(Ordering::Relaxed);
    let push_active = FFMPEG_PUSH_ACTIVE.load(Ordering::Relaxed);

    let (last_progress, stats_source) = if hls_cache_active && !push_active {
        let cache_progress = LAST_CACHE_PROGRESS_TIME.load(Ordering::Relaxed);
        if cache_progress > 0 {
            (cache_progress, "HLS 缓存写入")
        } else {
            (LAST_PROGRESS_TIME.load(Ordering::Relaxed), "HLS 缓存写入")
        }
    } else {
        let push_progress = LAST_PUSH_PROGRESS_TIME.load(Ordering::Relaxed);
        if push_progress > 0 {
            (push_progress, "推流")
        } else {
            (FFMPEG_PUSH_STARTED_AT.load(Ordering::Relaxed), "推流")
        }
    };

    if last_progress == 0 {
        // No progress recorded yet, not stuck
        return None;
    }

    // Check if we're getting stats updates
    let stats_elapsed = now.saturating_sub(last_progress);
    if stats_elapsed > timeout_secs as u32 {
        return Some(StuckReason::NoStats {
            elapsed_secs: stats_elapsed,
            source: stats_source,
        });
    }

    // In cached HLS mode the delayed reader may report a steady speed/bitrate
    // while its input playlist advances by segment cadence, so stream timestamp
    // freezes alone are not a reliable stuck signal during live playback.
    // Once transfer is idle, restore the pre-bfcccde stream-time check so
    // end-of-stream shutdown stays fast instead of waiting on low-speed decay.
    let push_bitrate_kbps = f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS).unwrap_or(0.0);
    let cache_bitrate_kbps = f32_from_atomic_bits(&FFMPEG_CACHE_BITRATE_KBPS).unwrap_or(0.0);
    let transfer_idle = is_network_transfer_idle(
        hls_cache_active,
        push_active,
        push_bitrate_kbps,
        cache_bitrate_kbps,
    );
    update_network_idle_tracking(transfer_idle);

    let check_stream_time_frozen = !hls_cache_active || (push_active && transfer_idle);

    if check_stream_time_frozen {
        let last_stream_update = LAST_STREAM_TIME_UPDATE.load(Ordering::Relaxed);
        if last_stream_update > 0 {
            let stream_time_elapsed = now.saturating_sub(last_stream_update);

            if stream_time_elapsed > STREAM_TIME_FROZEN_SECS {
                return Some(StuckReason::StreamTimeFrozen {
                    elapsed_secs: stream_time_elapsed,
                });
            }
        }
    }

    if transfer_idle {
        let idle_since = NETWORK_IDLE_SINCE.load(Ordering::Relaxed);
        if idle_since > 0 {
            let idle_elapsed = now.saturating_sub(idle_since);
            if idle_elapsed >= IDLE_DRAIN_TIMEOUT_SECS {
                return Some(StuckReason::IdleDrain {
                    elapsed_secs: idle_elapsed,
                });
            }
        }
    }

    // A decaying speed with zero TX/RX is normal end-of-stream drain, not a stall.
    if !transfer_idle {
        let low_speed_since = LOW_SPEED_SINCE.load(Ordering::Relaxed);
        if low_speed_since > 0 {
            let low_speed_elapsed = now.saturating_sub(low_speed_since);
            if low_speed_elapsed > LOW_SPEED_TIMEOUT_SECS {
                return Some(StuckReason::LowSpeed {
                    elapsed_secs: low_speed_elapsed,
                });
            }
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

/// Emergency execution fence. Unlike normal stop, cancels preparation/cleanup
/// waits and signals all owned publishing children without waiting for reaping.
/// The task survives cancellation of the watchdog or requesting HTTP handler.
pub async fn fence_ffmpeg() {
    let _ = tokio::spawn(async { FFMPEG_SUPERVISOR.fence().await }).await;
}

/// Internal stop function with manual flag
async fn stop_ffmpeg_internal(manual: bool) {
    stop_ffmpeg_matching(None, manual, async {}).await;
}

async fn stop_ffmpeg_session(session: FfmpegSession) -> bool {
    stop_ffmpeg_matching(Some(session), false, async {}).await
}

/// Keep replacement startup excluded until the session's external cleanup has
/// finished (for example stopping its Bilibili room).
pub async fn stop_ffmpeg_session_with(
    session: FfmpegSession,
    cleanup: impl Future<Output = ()> + Send + 'static,
) -> bool {
    stop_ffmpeg_matching(Some(session), false, cleanup).await
}

async fn stop_ffmpeg_matching(
    expected: Option<FfmpegSession>,
    manual: bool,
    cleanup: impl Future<Output = ()> + Send + 'static,
) -> bool {
    // The cleanup survives cancellation of the requesting session task. An
    // execution fence may interrupt it to stop every publisher promptly.
    tokio::spawn(async move {
        FFMPEG_SUPERVISOR
            .stop_matching(expected, manual, cleanup)
            .await
    })
    .await
    .unwrap_or(false)
}

/// Keep a stream running when preparation fails, and fence the entire
/// preparation/stop sequence against replacement by another session.
pub async fn transition_ffmpeg_session(
    session: FfmpegSession,
    prepare: impl Future<Output = bool> + Send + 'static,
) -> bool {
    tokio::spawn(async move { FFMPEG_SUPERVISOR.transition(session, prepare).await })
        .await
        .unwrap_or(false)
}

async fn stop_locked(supervisor: &mut Option<FfmpegProcess>, reason: Option<&str>) {
    if let Some(mut process) = supervisor.take() {
        // Abort readers/monitors before clearing shared state. Every child
        // also has kill_on_drop enabled in case this runtime shuts down.
        drop(std::mem::take(&mut process.tasks));
        if let Err(error) = process.kill().await {
            tracing::warn!(
                "{} 子进程清理失败；{}",
                process.log,
                safe_diagnostic(&error.to_string())
            );
        } else if let Some(reason) = reason {
            // Ingest cleanup reports its own failures; kill() only confirms
            // that every ffmpeg child stopped successfully.
            let stopped_roles = process
                .children
                .iter()
                .map(|child| child.log.role.to_string())
                .collect::<Vec<_>>()
                .join("/");
            tracing::info!(
                "{} ffmpeg 子进程已停止；原因={reason}，角色={stopped_roles}",
                process.log,
            );
        }
        if let Some(cache_dir) = process.cache_dir.take() {
            if let Err(error) = tokio::fs::remove_dir_all(&cache_dir).await {
                tracing::warn!(
                    "{} 缓存清理失败；{}",
                    process.log,
                    safe_diagnostic(&error.to_string())
                );
            }
        }
    }
    reset_stopped_tracking_state();
}

fn reset_stopped_tracking_state() {
    // Clear speed, progress time, and rendered stats when ffmpeg stops.
    reset_stats_display();
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_PUSH_FPS.store(OPTIONAL_F32_NONE_BITS, Ordering::Relaxed);
    FFMPEG_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_PUSH_FRAME.store(OPTIONAL_U64_NONE, Ordering::Relaxed);
    FFMPEG_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_HLS_CACHE_ACTIVE.store(false, Ordering::Relaxed);
    FFMPEG_PUSH_ACTIVE.store(false, Ordering::Relaxed);
    FFMPEG_PUSH_STARTED_AT.store(0, Ordering::Relaxed);
    LAST_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_PUSH_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_CACHE_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TIME_SECS.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
    NETWORK_IDLE_SINCE.store(0, Ordering::Relaxed);
}

const NETWORK_PANEL_WIDTH: usize = 72;
const NETWORK_PANEL_CONTENT_WIDTH: usize = NETWORK_PANEL_WIDTH - 4;
const NETWORK_GRAPH_WIDTH: usize = NETWORK_PANEL_CONTENT_WIDTH - 9;
const NETWORK_HISTORY_LIMIT: usize = 60;
const NETWORK_SAMPLE_GAP_LIMIT_MS: u64 = 10_000;
const NETWORK_MIN_SAMPLE_INTERVAL_MS: u64 = 250;
const NETWORK_HISTORY_SAMPLE_MS: u64 = 1000;
const NETWORK_HISTORY_WINDOW_SECS: usize =
    (NETWORK_HISTORY_LIMIT as u64 * NETWORK_HISTORY_SAMPLE_MS / 1000) as usize;
const CACHE_BYTE_SAMPLE_INTERVAL_MS: u64 = 1000;
const CACHE_RATE_WINDOW_MS: u64 = 5_000;
const OPTIONAL_F32_NONE_BITS: u32 = u32::MAX;
const OPTIONAL_U64_NONE: u64 = u64::MAX;

#[derive(Clone, Copy)]
enum FfmpegStatsRole {
    Cache,
    Push,
}

#[derive(Clone, Default)]
struct FfmpegStatsSample {
    frame: Option<u64>,
    fps: Option<f32>,
    time: Option<String>,
    output_size_bytes: Option<u64>,
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
    last_history_sample_ms: u64,
    rendered_lines: usize,
    enabled: bool,
}

impl FfmpegStatsDisplay {
    fn reset(&mut self) {
        self.clear_rendered();
        *self = Self {
            enabled: std::io::stderr().is_terminal(),
            ..Self::default()
        };
    }

    fn clear_rendered(&mut self) {
        if self.rendered_lines > 0 && self.enabled {
            eprint!("\x1b[?25l\x1b[{}F\x1b[J\x1b[?25h", self.rendered_lines);
            let _ = std::io::stderr().flush();
        }
        self.rendered_lines = 0;
    }

    fn update(&mut self, role: FfmpegStatsRole, sample: FfmpegStatsSample) {
        match role {
            FfmpegStatsRole::Cache => {
                let merged = self.cache.get_or_insert_with(FfmpegStatsSample::default);
                if sample.time.is_some() {
                    merged.time = sample.time;
                    merged.stream_time_secs = sample.stream_time_secs;
                }
                if sample.speed.is_some() {
                    merged.speed = sample.speed;
                }
                if sample.frame.is_some() {
                    merged.frame = sample.frame;
                }
                if sample.fps.is_some() {
                    merged.fps = sample.fps;
                }
                if sample.bitrate_kbps.is_some() {
                    merged.bitrate = sample.bitrate;
                    merged.bitrate_kbps = sample.bitrate_kbps;
                }
            }
            FfmpegStatsRole::Push => {
                let merged = self.push.get_or_insert_with(FfmpegStatsSample::default);
                merge_stats_sample(merged, sample);
            }
        }
        self.record();
    }

    fn update_cache_bitrate(&mut self, bitrate_kbps: f32) {
        let sample = self.cache.get_or_insert_with(FfmpegStatsSample::default);
        if bitrate_kbps > 0.0 {
            sample.bitrate = Some(format_network_rate(bitrate_kbps));
            sample.bitrate_kbps = Some(bitrate_kbps);
        } else {
            sample.bitrate = None;
            sample.bitrate_kbps = None;
        }
        self.record();
    }

    fn record(&mut self) {
        self.sample_history();
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
                "Auto scale {:>12}  {:>4}",
                format_network_rate(scale_kbps),
                format!("{}s", NETWORK_HISTORY_WINDOW_SECS)
            )),
            network_content_line(&Self::meter_row("Cache RX", self.cache.as_ref(), false)),
            network_content_line(&Self::rx_graph_row("RX", &self.cache_history, scale_kbps)),
            network_content_line(&Self::zero_axis_row()),
            network_content_line(&Self::tx_graph_row("TX", &self.push_history, scale_kbps)),
            network_content_line(&Self::meter_row("RTMP TX", self.push.as_ref(), true)),
            network_bottom_border(),
        ];

        let mut output = String::new();
        output.push_str("\x1b[?25l");
        if self.rendered_lines > 0 {
            output.push_str(&format!("\x1b[{}F", self.rendered_lines));
        }
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                output.push_str(&format!("\x1b[2K\x1b[1m{}\n", line));
            } else if index == lines.len() - 1 {
                output.push_str(&format!("\x1b[2K{}\x1b[0m\n", line));
            } else {
                output.push_str(&format!("\x1b[2K{}\n", line));
            }
        }
        output.push_str("\x1b[?25h");
        eprint!("{}", output);
        let _ = std::io::stderr().flush();
        self.rendered_lines = lines.len();
    }

    fn sample_history(&mut self) {
        let now = now_millis();
        if self.last_history_sample_ms != 0
            && now.saturating_sub(self.last_history_sample_ms) < NETWORK_HISTORY_SAMPLE_MS
        {
            return;
        }
        self.last_history_sample_ms = now;
        push_history_sample(
            &mut self.cache_history,
            self.cache
                .as_ref()
                .and_then(|s| s.bitrate_kbps)
                .unwrap_or(0.0),
        );
        push_history_sample(
            &mut self.push_history,
            self.push
                .as_ref()
                .and_then(|s| s.bitrate_kbps)
                .unwrap_or(0.0),
        );
    }

    fn meter_row(label: &str, sample: Option<&FfmpegStatsSample>, show_fps: bool) -> String {
        let bitrate = sample
            .and_then(|s| s.bitrate_kbps)
            .map(format_network_rate)
            .unwrap_or_else(|| "-".to_string());
        let speed = sample
            .and_then(|s| s.speed.map(|v| format!("{:.2}x", v)))
            .unwrap_or_else(|| "-".to_string());
        let time = sample
            .and_then(|s| s.stream_time_secs)
            .map(format_stream_time)
            .unwrap_or_else(|| "-".to_string());
        let line = if show_fps {
            let fps = sample
                .and_then(|s| s.fps.filter(|fps| fps.is_finite() && *fps >= 0.0))
                .map(format_fps_label)
                .unwrap_or_else(|| "- fps".to_string());
            format!(
                "{:<8} {:>12}  {:>6}  {:>8}  {:>8}",
                label, bitrate, speed, time, fps
            )
        } else {
            format!("{:<8} {:>12}  {:>6}  {:>8}", label, bitrate, speed, time)
        };
        truncate_cell(&line, NETWORK_PANEL_CONTENT_WIDTH)
    }

    fn rx_graph_row(label: &str, history: &[f32], scale_kbps: f32) -> String {
        format!(
            "{:<6} ↑{}",
            label,
            mirrored_sparkline(history, NETWORK_GRAPH_WIDTH, scale_kbps, GraphDirection::Up)
        )
    }

    fn zero_axis_row() -> String {
        format!("{:<6} ┼{}", "0", "─".repeat(NETWORK_GRAPH_WIDTH))
    }

    fn tx_graph_row(label: &str, history: &[f32], scale_kbps: f32) -> String {
        format!(
            "{:<6} ↓{}",
            label,
            mirrored_sparkline(
                history,
                NETWORK_GRAPH_WIDTH,
                scale_kbps,
                GraphDirection::Down
            )
        )
    }
}

fn merge_stats_sample(target: &mut FfmpegStatsSample, sample: FfmpegStatsSample) {
    if sample.time.is_some() {
        target.time = sample.time;
        target.stream_time_secs = sample.stream_time_secs;
    }
    if sample.output_size_bytes.is_some() {
        target.output_size_bytes = sample.output_size_bytes;
    }
    if sample.speed.is_some() {
        target.speed = sample.speed;
    }
    if sample.frame.is_some() {
        target.frame = sample.frame;
    }
    if sample.fps.is_some() {
        target.fps = sample.fps;
    }
    if sample.bitrate_kbps.is_some() {
        target.bitrate = sample.bitrate;
        target.bitrate_kbps = sample.bitrate_kbps;
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

fn snapshot_bitrate_history() -> (Vec<f32>, Vec<f32>) {
    let display = match FFMPEG_STATS_DISPLAY.lock() {
        Ok(display) => display,
        Err(poisoned) => {
            tracing::warn!("Recovering poisoned ffmpeg stats display");
            poisoned.into_inner()
        }
    };
    (display.push_history.clone(), display.cache_history.clone())
}

#[derive(Clone, Copy)]
enum GraphDirection {
    Up,
    Down,
}

fn mirrored_sparkline(
    history: &[f32],
    width: usize,
    scale_kbps: f32,
    direction: GraphDirection,
) -> String {
    const UP_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    const DOWN_BARS: [char; 8] = ['▔', '🮂', '🮃', '▀', '🮄', '🮅', '🮆', '█'];
    if history.is_empty() {
        return " ".repeat(width);
    }

    let bars = match direction {
        GraphDirection::Up => UP_BARS,
        GraphDirection::Down => DOWN_BARS,
    };
    let start = history.len().saturating_sub(width);
    let mut output = String::with_capacity(width);
    for _ in 0..width.saturating_sub(history.len() - start) {
        output.push(' ');
    }
    for value in &history[start..] {
        if *value <= 0.0 {
            output.push(' ');
            continue;
        }
        let ratio = if scale_kbps > 0.0 {
            (value / scale_kbps).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let index = (ratio * (bars.len() - 1) as f32).round() as usize;
        output.push(bars[index]);
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

fn format_stream_time(secs: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn format_fps_label(fps: f32) -> String {
    if fps >= 100.0 {
        format!("{:.0} fps", fps)
    } else {
        format!("{:.1} fps", fps)
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

pub fn clear_ffmpeg_stats_display() {
    if let Ok(mut display) = FFMPEG_STATS_DISPLAY.lock() {
        display.clear_rendered();
    }
}

fn update_stats_display(role: FfmpegStatsRole, sample: FfmpegStatsSample) {
    if let Ok(mut display) = FFMPEG_STATS_DISPLAY.lock() {
        display.update(role, sample);
    }
}

fn update_cache_bitrate_display(bitrate_kbps: f32) {
    if let Ok(mut display) = FFMPEG_STATS_DISPLAY.lock() {
        display.update_cache_bitrate(bitrate_kbps);
    }
}

fn now_millis() -> u64 {
    unix_time_millis_from(std::time::SystemTime::now())
}

fn unix_time_millis_from(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
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

fn update_network_counters_from_total_bytes(
    role: FfmpegStatsRole,
    total_bytes: u64,
) -> Option<f32> {
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

    let previous_bitrate = f32_from_atomic_bits(bitrate);
    let previous_total = total.load(Ordering::Relaxed);
    let previous_sample = last_sample.load(Ordering::Relaxed);
    if previous_total == 0 || previous_sample == 0 {
        total.store(total_bytes, Ordering::Relaxed);
        last_sample.store(now, Ordering::Relaxed);
        return previous_bitrate;
    }

    if total_bytes < previous_total {
        tracing::debug!(
            "Ignoring non-monotonic ffmpeg output size sample: {} < {}",
            total_bytes,
            previous_total
        );
        return previous_bitrate;
    }

    let elapsed_ms = now.saturating_sub(previous_sample);
    if !(NETWORK_MIN_SAMPLE_INTERVAL_MS..=NETWORK_SAMPLE_GAP_LIMIT_MS).contains(&elapsed_ms) {
        total.store(total_bytes, Ordering::Relaxed);
        last_sample.store(now, Ordering::Relaxed);
        return previous_bitrate;
    }

    total.store(total_bytes, Ordering::Relaxed);
    last_sample.store(now, Ordering::Relaxed);

    let delta_bytes = total_bytes - previous_total;
    let kbps = delta_bytes as f32 * 8.0 / elapsed_ms as f32;
    if kbps.is_finite() {
        bitrate.store(kbps.to_bits(), Ordering::Relaxed);
        Some(kbps)
    } else {
        previous_bitrate
    }
}

fn update_network_counters_from_bytes(
    role: FfmpegStatsRole,
    bitrate_kbps: Option<f32>,
    delta_bytes: u64,
) {
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

    if let Some(kbps) = bitrate_kbps {
        bitrate.store(kbps.to_bits(), Ordering::Relaxed);
    }
    if delta_bytes > 0 {
        total.fetch_add(delta_bytes, Ordering::Relaxed);
    }
    last_sample.store(now, Ordering::Relaxed);
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

fn parse_size_to_bytes(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed == "N/A" {
        return None;
    }

    let number: String = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let parsed = number.parse::<f64>().ok()?;
    let unit = trimmed[number.len()..].trim().to_ascii_lowercase();

    let multiplier = if unit.starts_with("gib") {
        1024.0 * 1024.0 * 1024.0
    } else if unit.starts_with("mib") {
        1024.0 * 1024.0
    } else if unit.starts_with("kib") {
        1024.0
    } else if unit.starts_with("gb") {
        1024.0 * 1024.0 * 1024.0
    } else if unit.starts_with("mb") {
        1024.0 * 1024.0
    } else if unit.starts_with("kb") {
        1024.0
    } else {
        1.0
    };

    let bytes = parsed * multiplier;
    if bytes.is_finite() && bytes >= 0.0 {
        Some(bytes.round() as u64)
    } else {
        None
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
        if let Some(value) = value_after_key(&parts, idx, "frame=") {
            sample.frame = value.parse::<u64>().ok();
        } else if let Some(value) = value_after_key(&parts, idx, "fps=") {
            sample.fps = value.parse::<f32>().ok();
        } else if let Some(value) = value_after_key(&parts, idx, "size=") {
            sample.output_size_bytes = parse_size_to_bytes(value);
        } else if let Some(value) = value_after_key(&parts, idx, "time=") {
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
    let now_ms = now_millis();
    let cache_dir = std::env::temp_dir().join(format!(
        "bilistream-hls-cache-{}-{}",
        std::process::id(),
        now_ms
    ));
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

struct CacheFileSize {
    size: u64,
    seen_generation: u64,
}

struct CacheByteTracker {
    cache_dir: PathBuf,
    file_sizes: HashMap<PathBuf, CacheFileSize>,
    scan_generation: u64,
    last_sample_ms: Option<u64>,
    samples: VecDeque<(u64, u64)>,
}

impl CacheByteTracker {
    fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            file_sizes: HashMap::new(),
            scan_generation: 0,
            last_sample_ms: None,
            samples: VecDeque::new(),
        }
    }

    fn record_file_size(&mut self, path: PathBuf, size: u64, generation: u64) -> u64 {
        let cached = self.file_sizes.entry(path).or_insert(CacheFileSize {
            size: 0,
            seen_generation: generation,
        });
        let delta = size.saturating_sub(cached.size);
        cached.size = size;
        cached.seen_generation = generation;
        delta
    }

    fn prune_unseen_files(&mut self, generation: u64) {
        self.file_sizes
            .retain(|_, cached| cached.seen_generation == generation);
    }

    fn sample(&mut self) -> Option<(Option<f32>, u64)> {
        let now = now_millis();
        let mut delta_bytes = 0_u64;
        self.scan_generation = match self.scan_generation.checked_add(1) {
            Some(generation) => generation,
            None => {
                self.file_sizes.clear();
                1
            }
        };
        let generation = self.scan_generation;

        let entries = std::fs::read_dir(&self.cache_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let size = metadata.len();
            delta_bytes = delta_bytes.saturating_add(self.record_file_size(path, size, generation));
        }

        self.prune_unseen_files(generation);

        if let Some(last_sample_ms) = self.last_sample_ms {
            if now.saturating_sub(last_sample_ms) > NETWORK_SAMPLE_GAP_LIMIT_MS {
                self.samples.clear();
            }
        }
        self.last_sample_ms = Some(now);
        self.samples.push_back((now, delta_bytes));

        while let Some((sample_ms, _)) = self.samples.front() {
            if now.saturating_sub(*sample_ms) <= CACHE_RATE_WINDOW_MS {
                break;
            }
            self.samples.pop_front();
        }

        let bitrate_kbps = if let Some((window_start_ms, _)) = self.samples.front() {
            let elapsed_ms = now.saturating_sub(*window_start_ms);
            if elapsed_ms > 0 {
                let window_bytes: u64 = self.samples.iter().map(|(_, bytes)| *bytes).sum();
                Some(window_bytes as f32 * 8.0 / elapsed_ms as f32)
            } else {
                None
            }
        } else {
            None
        };

        Some((bitrate_kbps, delta_bytes))
    }
}

fn start_hls_cache_byte_monitor(session: FfmpegSession, cache_dir: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracker = CacheByteTracker::new(cache_dir.clone());
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(
            CACHE_BYTE_SAMPLE_INTERVAL_MS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let Ok((next_tracker, sample)) = tokio::task::spawn_blocking(move || {
                let sample = tracker.sample();
                (tracker, sample)
            })
            .await
            else {
                break;
            };
            tracker = next_tracker;
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            if matching_process(&mut supervisor, session).is_none() {
                break;
            }
            let Some((bitrate_kbps, delta_bytes)) = sample else {
                continue;
            };
            update_network_counters_from_bytes(FfmpegStatsRole::Cache, bitrate_kbps, delta_bytes);
            if let Some(kbps) = bitrate_kbps {
                update_cache_bitrate_display(kbps);
            }
        }
    })
}

const STDERR_LINE_LIMIT: usize = 64 * 1024;
const STDERR_DIAGNOSTIC_CLASSES: usize = 16;
const STDERR_REPEAT_INTERVAL: Duration = Duration::from_secs(300);

/// Keep bytes until a delimiter so reads cannot split a UTF-8 character. Once
/// full, discard through the delimiter: a credential tail is never a new line.
#[derive(Default)]
struct StderrLines {
    bytes: Vec<u8>,
    truncated: bool,
}

impl StderrLines {
    fn push(&mut self, chunk: &[u8], mut line: impl FnMut(&[u8], bool)) {
        for &byte in chunk {
            if matches!(byte, b'\r' | b'\n') {
                self.finish(&mut line);
            } else if self.bytes.len() < STDERR_LINE_LIMIT {
                self.bytes.push(byte);
            } else {
                self.truncated = true;
            }
        }
    }

    fn finish(&mut self, mut line: impl FnMut(&[u8], bool)) {
        if !self.bytes.is_empty() || self.truncated {
            line(&self.bytes, self.truncated);
            self.bytes.clear();
            self.truncated = false;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagnosticLevel {
    Error,
    Warn,
    Debug,
}

fn native_diagnostic_class(lower: &str) -> Option<String> {
    for (needle, class) in [
        ("broken pipe", "broken pipe"),
        ("conversion failed", "conversion failed"),
        ("connection reset", "connection reset"),
        ("connection refused", "connection refused"),
        ("timed out", "timeout"),
        ("invalid data found", "invalid data"),
        ("end of file", "end of file"),
        ("permission denied", "permission denied"),
        ("invalid argument", "invalid argument"),
        ("input/output error", "input/output error"),
        ("no route to host", "no route to host"),
        ("no space left on device", "no space left on device"),
        ("protocol not found", "protocol not found"),
    ] {
        if lower.contains(needle) {
            return Some(class.to_string());
        }
    }
    for prefix in ["http error ", "server returned ", "http/1.1 ", "http/2 "] {
        if let Some((_, rest)) = lower.split_once(prefix) {
            let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if code
                .parse::<u16>()
                .is_ok_and(|code| (400..600).contains(&code))
            {
                return Some(format!("HTTP {code}"));
            }
        }
    }
    None
}

struct StderrReporter {
    log: ChildLogContext,
    publishing_observed: bool,
    sensitive_continuation: bool,
    reported: VecDeque<(DiagnosticLevel, String)>,
    suppressed: u64,
    suppressed_level: DiagnosticLevel,
    last_diagnostic: String,
    last_summary: Instant,
}

impl StderrReporter {
    fn new(log: ChildLogContext) -> Self {
        Self {
            last_summary: log.started,
            log,
            publishing_observed: false,
            sensitive_continuation: false,
            reported: VecDeque::new(),
            suppressed: 0,
            suppressed_level: DiagnosticLevel::Warn,
            last_diagnostic: String::new(),
        }
    }

    fn diagnostic(&mut self, line: &str, log_level: &str, truncated: bool, now: Instant) {
        let continuation = self.sensitive_continuation;
        self.sensitive_continuation |= truncated || has_sensitive_data(line);
        let lower = line.to_ascii_lowercase();
        let class = native_diagnostic_class(&lower);
        let level = if is_harmless_hls_keepalive_warning(&lower) {
            DiagnosticLevel::Debug
        } else if lower.contains("error") {
            DiagnosticLevel::Error
        } else if lower.contains("warning") {
            DiagnosticLevel::Warn
        } else if class.is_some() {
            DiagnosticLevel::Error
        } else if truncated {
            DiagnosticLevel::Warn
        } else if log_level == "debug" || log_level == "info" {
            DiagnosticLevel::Debug
        } else {
            return;
        };
        let mut diagnostic = if continuation {
            safe_diagnostic_continuation(line)
        } else {
            safe_diagnostic(line)
        };
        if truncated {
            diagnostic.push_str(" …[原始行超过64 KiB，已截断]");
        }
        if level != DiagnosticLevel::Debug {
            // Keys are bounded, sanitized error classes owned by this child.
            // Never include a moving URL or another session in suppression.
            // A warning must not suppress a later error of the same class.
            let key = (
                level,
                class.unwrap_or_else(|| diagnostic.to_ascii_lowercase()),
            );
            if self.reported.contains(&key) {
                self.suppressed += 1;
                self.last_diagnostic = diagnostic;
                if level == DiagnosticLevel::Error {
                    self.suppressed_level = level;
                }
                if now.duration_since(self.last_summary) >= STDERR_REPEAT_INTERVAL {
                    self.repeat_summary("诊断仍在重复", now);
                }
                return;
            }
            if self.reported.len() == STDERR_DIAGNOSTIC_CLASSES {
                self.reported.pop_front();
            }
            self.reported.push_back(key);
        }
        self.emit(level, &diagnostic, now);
    }

    fn emit(&self, level: DiagnosticLevel, diagnostic: &str, now: Instant) {
        let elapsed = human_duration(now.duration_since(self.log.started).as_secs() as i64);
        match level {
            DiagnosticLevel::Error => tracing::error!("{}；{diagnostic}；运行{elapsed}", self.log),
            DiagnosticLevel::Warn => tracing::warn!("{}；{diagnostic}；运行{elapsed}", self.log),
            DiagnosticLevel::Debug => tracing::debug!("{}；{diagnostic}；运行{elapsed}", self.log),
        }
    }

    fn repeat_summary(&mut self, state: &str, now: Instant) {
        if self.suppressed > 0 {
            self.emit(
                self.suppressed_level,
                &format!(
                    "{state}；已合并{}条重复诊断：{}",
                    self.suppressed, self.last_diagnostic
                ),
                now,
            );
            self.suppressed = 0;
            self.suppressed_level = DiagnosticLevel::Warn;
            self.last_summary = now;
        }
    }

    fn observe_publishing(&mut self, positive_push_sample: bool) {
        if positive_push_sample && !self.publishing_observed {
            self.publishing_observed = true;
            tracing::info!("{} 已观察到推流输出", self.log);
        }
    }
}

fn spawn_ffmpeg_stderr_monitor(
    stderr: tokio::process::ChildStderr,
    log_level: String,
    context: ChildLogContext,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        monitor_ffmpeg_stderr(&FFMPEG_SUPERVISOR, stderr, &log_level, context).await;
    })
}

async fn monitor_ffmpeg_stderr(
    supervisor: &ProcessSupervisor,
    mut stderr: impl tokio::io::AsyncRead + Unpin,
    log_level: &str,
    context: ChildLogContext,
) {
    use tokio::io::AsyncReadExt;

    let session = context.session.session;
    let mut reporter = StderrReporter::new(context);
    let mut buffer = vec![0u8; 8192];
    let mut lines = StderrLines::default();
    loop {
        let read = stderr.read(&mut buffer).await;
        let mut process = supervisor.lock().await;
        if matching_process(&mut process, session).is_none() {
            break;
        }
        let mut handle = |bytes: &[u8], truncated| {
            handle_ffmpeg_stderr_line(
                &String::from_utf8_lossy(bytes),
                log_level,
                &mut reporter,
                truncated,
            );
        };
        match read {
            Ok(0) | Err(_) => {
                // Only an owned reader reaching EOF/error flushes. Drop/abort
                // deliberately has no flush and never waits for a shutdown tail.
                lines.finish(&mut handle);
                if let Err(error) = read {
                    tracing::warn!(
                        "{} 读取诊断失败；{}",
                        reporter.log,
                        safe_diagnostic(&error.to_string())
                    );
                }
                reporter.repeat_summary("诊断读取结束", Instant::now());
                break;
            }
            Ok(n) => lines.push(&buffer[..n], &mut handle),
        }
    }
}

fn handle_ffmpeg_stderr_line(
    line: &str,
    log_level: &str,
    reporter: &mut StderrReporter,
    truncated: bool,
) {
    if line.trim().is_empty() {
        return;
    }

    if let Some(mut sample) = extract_ffmpeg_stats(line) {
        // Only an intact stats record without credential text is a safe new
        // boundary. Stats and a wrapped/truncated diagnostic can share a line.
        // Preserve all progress updates even when its diagnostic tail is unsafe.
        reporter.sensitive_continuation = truncated || has_sensitive_data(line);
        let stats_role = reporter.log.role.stats_role();
        update_progress_time(stats_role);
        if let Some(role) = stats_role {
            if matches!(role, FfmpegStatsRole::Cache) {
                sample.bitrate = None;
                sample.bitrate_kbps = None;
            } else if let Some(output_size_bytes) = sample.output_size_bytes {
                if let Some(kbps) =
                    update_network_counters_from_total_bytes(role, output_size_bytes)
                {
                    sample.bitrate = Some(format_network_rate(kbps));
                    sample.bitrate_kbps = Some(kbps);
                }
            } else if let Some(bitrate_kbps) = sample.bitrate_kbps {
                update_network_counters(role, bitrate_kbps);
            }
            match role {
                FfmpegStatsRole::Cache => {
                    if let Some(speed) = sample.speed {
                        FFMPEG_CACHE_SPEED.store(speed.to_bits(), Ordering::Relaxed);
                    }
                    if let Some(stream_time) = sample.stream_time_secs {
                        FFMPEG_CACHE_TIME_SECS.store(stream_time, Ordering::Relaxed);
                    }
                }
                FfmpegStatsRole::Push => {
                    if let Some(frame) = sample.frame {
                        FFMPEG_PUSH_FRAME.store(frame, Ordering::Relaxed);
                    }
                    if let Some(fps) = sample.fps.filter(|fps| fps.is_finite()) {
                        FFMPEG_PUSH_FPS.store(fps.to_bits(), Ordering::Relaxed);
                    }
                    if let Some(speed) = sample.speed {
                        FFMPEG_SPEED.store(speed.to_bits(), Ordering::Relaxed);
                        update_speed_tracking(speed);
                    }
                    if let Some(stream_time) = sample.stream_time_secs {
                        update_stream_time(stream_time);
                    }
                }
            }
            let positive_push_sample = matches!(role, FfmpegStatsRole::Push)
                && (sample.bitrate_kbps.is_some_and(|value| value > 0.0)
                    || sample.speed.is_some_and(|value| value > 0.0));
            update_stats_display(role, sample);
            reporter.observe_publishing(positive_push_sample);
        }
    } else {
        reporter.diagnostic(line, log_level, truncated, Instant::now());
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

fn is_harmless_hls_keepalive_warning(lower: &str) -> bool {
    lower.contains("keepalive request failed")
        || lower.contains("cannot reuse http connection for different host")
}

fn append_crop_or_copy(cmd: &mut Command, crop: Option<(u32, u32, u32, u32)>) {
    if let Some((width, height, x, y)) = crop {
        tracing::debug!("应用裁剪滤镜: {}:{}:{}:{}", width, height, x, y);
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

fn append_flv_output_options(cmd: &mut Command) {
    cmd.arg("-max_muxing_queue_size")
        .arg("8192")
        .arg("-flvflags")
        .arg("no_duration_filesize")
        .arg("-muxdelay")
        .arg("0")
        .arg("-muxpreload")
        .arg("0")
        .arg("-f")
        .arg("flv");
}

fn reset_ffmpeg_tracking_state() {
    reset_stats_display();
    update_progress_time(None);
    LAST_PUSH_PROGRESS_TIME.store(0, Ordering::Relaxed);
    LAST_CACHE_PROGRESS_TIME.store(0, Ordering::Relaxed);
    FFMPEG_PUSH_ACTIVE.store(false, Ordering::Relaxed);
    FFMPEG_PUSH_STARTED_AT.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TIME_SECS.store(0, Ordering::Relaxed);
    LAST_STREAM_TIME_UPDATE.store(0, Ordering::Relaxed);
    FFMPEG_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_SPEED.store(0, Ordering::Relaxed);
    FFMPEG_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_BITRATE_KBPS.store(0, Ordering::Relaxed);
    FFMPEG_PUSH_FPS.store(OPTIONAL_F32_NONE_BITS, Ordering::Relaxed);
    FFMPEG_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_TOTAL_BYTES.store(0, Ordering::Relaxed);
    FFMPEG_PUSH_FRAME.store(OPTIONAL_U64_NONE, Ordering::Relaxed);
    FFMPEG_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    FFMPEG_CACHE_LAST_SAMPLE_MS.store(0, Ordering::Relaxed);
    LOW_SPEED_SINCE.store(0, Ordering::Relaxed);
    NETWORK_IDLE_SINCE.store(0, Ordering::Relaxed);
}

fn start_ffmpeg_timeout_monitor(session: FfmpegSession, timeout_secs: u64) -> JoinHandle<()> {
    tokio::spawn(monitor_ffmpeg_timeout(session, timeout_secs))
}

async fn spawn_direct_ffmpeg(
    supervisor: &mut Option<FfmpegProcess>,
    rtmp_url_key: String,
    source: FfmpegSource,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>,
    identity: FfmpegStreamIdentity,
) {
    let ffmpeg_cmd = get_ffmpeg_command();

    let session = FfmpegSession(NEXT_SESSION.fetch_add(1, Ordering::Relaxed));
    let context = SessionLogContext::new(identity, session);
    tracing::debug!("{context} HLS 缓存关闭，准备直接推流");
    let mut tasks = SessionTasks::default();
    let ingest_spawned = match &source {
        FfmpegSource::Piped(piped) => {
            spawn_piped_ingest(piped, &log_level, &context, &mut tasks).await
        }
        FfmpegSource::HlsUrl(_) => None,
    };
    if matches!(source, FfmpegSource::Piped(_)) && ingest_spawned.is_none() {
        return;
    }
    let (ingest_child, ingest_stdout) = match ingest_spawned {
        Some((child, stdout)) => (Some(child), Some(stdout)),
        None => (None, None),
    };

    let mut cmd = Command::new(&ffmpeg_cmd);
    configure_tokio_no_window(&mut cmd);

    cmd.arg("-nostdin")
        .arg("-stats")
        .arg("-loglevel")
        .arg(&log_level);
    append_source_input(&mut cmd, &source, proxy.as_deref(), true);

    append_crop_or_copy(&mut cmd, crop);

    append_flv_output_options(&mut cmd);
    cmd.arg(rtmp_url_key)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(stdout) = ingest_stdout {
        if !pipe_ingest_stdout(&mut cmd, stdout, &context) {
            if let Some(mut ingest) = ingest_child {
                kill_child_tree(&mut ingest).await;
            }
            return;
        }
    }

    match FFMPEG_SUPERVISOR.spawn(&mut cmd) {
        Ok(mut child) => {
            let pid = child.id();
            let log = ChildLogContext::new(&context, ChildRole::Ffmpeg(FfmpegStatsRole::Push), pid);
            log.started();

            if let Some(pid_value) = pid {
                set_high_priority(pid_value);
            }

            reset_ffmpeg_tracking_state();
            FFMPEG_HLS_CACHE_ACTIVE.store(false, Ordering::Relaxed);
            mark_push_process_started();

            if let Some(stderr) = child.stderr.take() {
                tasks
                    .0
                    .push(spawn_ffmpeg_stderr_monitor(stderr, log_level, log.clone()));
            }

            let process = FfmpegProcess {
                session,
                log: context.clone(),
                tasks,
                children: vec![FfmpegChild { child, log }],
                ingest: ingest_child,
                pid,
                cache_dir: None,
            };
            *supervisor = Some(process);

            if let Some(process) = supervisor.as_mut() {
                process.tasks.0.push(start_ffmpeg_timeout_monitor(
                    session,
                    STARTUP_NO_STATS_TIMEOUT_SECS,
                ));
            }
        }
        Err(e) => {
            tracing::error!(
                "{context} 推流进程启动失败；{}",
                safe_diagnostic(&e.to_string())
            );
            if let Some(mut ingest) = ingest_child {
                kill_child_tree(&mut ingest).await;
            }
        }
    }
}

async fn spawn_cached_ffmpeg(
    supervisor: &mut Option<FfmpegProcess>,
    rtmp_url_key: String,
    source: FfmpegSource,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>,
    latency_secs: u64,
    identity: FfmpegStreamIdentity,
) {
    let cache_dir = match create_hls_cache_dir() {
        Ok(path) => path,
        Err(e) => {
            tracing::error!(
                "{identity} 缓存目录创建失败；{}",
                safe_diagnostic(&e.to_string())
            );
            return;
        }
    };
    let playlist_path = cache_dir.join("index.m3u8");
    let segment_pattern = cache_dir.join("segment_%06d.ts");
    let ffmpeg_cmd = get_ffmpeg_command();

    let session = FfmpegSession(NEXT_SESSION.fetch_add(1, Ordering::Relaxed));
    let context = SessionLogContext::new(identity, session);
    tracing::debug!(
        "{context} HLS 缓存启用；目标延迟={}",
        human_duration(latency_secs as i64)
    );
    let mut tasks = SessionTasks::default();
    let ingest_spawned = match &source {
        FfmpegSource::Piped(piped) => {
            spawn_piped_ingest(piped, &log_level, &context, &mut tasks).await
        }
        FfmpegSource::HlsUrl(_) => None,
    };
    if matches!(source, FfmpegSource::Piped(_)) && ingest_spawned.is_none() {
        return;
    }
    let (ingest_child, ingest_stdout) = match ingest_spawned {
        Some((child, stdout)) => (Some(child), Some(stdout)),
        None => (None, None),
    };

    let mut cache_cmd = Command::new(&ffmpeg_cmd);
    configure_tokio_no_window(&mut cache_cmd);

    cache_cmd
        .arg("-nostdin")
        .arg("-stats")
        .arg("-loglevel")
        .arg("warning")
        .arg("-rtbufsize")
        .arg("100M");
    append_source_input(&mut cache_cmd, &source, proxy.as_deref(), false);
    cache_cmd
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

    cache_cmd
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(stdout) = ingest_stdout {
        if !pipe_ingest_stdout(&mut cache_cmd, stdout, &context) {
            if let Some(mut ingest) = ingest_child {
                kill_child_tree(&mut ingest).await;
            }
            return;
        }
    }

    match FFMPEG_SUPERVISOR.spawn(&mut cache_cmd) {
        Ok(mut cache_child) => {
            let cache_pid = cache_child.id();
            let log = ChildLogContext::new(
                &context,
                ChildRole::Ffmpeg(FfmpegStatsRole::Cache),
                cache_pid,
            );
            log.started();

            if let Some(pid_value) = cache_pid {
                set_high_priority(pid_value);
            }

            reset_ffmpeg_tracking_state();
            FFMPEG_HLS_CACHE_ACTIVE.store(true, Ordering::Relaxed);
            tasks
                .0
                .push(start_hls_cache_byte_monitor(session, cache_dir.clone()));

            if let Some(stderr) = cache_child.stderr.take() {
                tasks.0.push(spawn_ffmpeg_stderr_monitor(
                    stderr,
                    log_level.clone(),
                    log.clone(),
                ));
            }

            let process = FfmpegProcess {
                session,
                log: context.clone(),
                tasks,
                children: vec![FfmpegChild {
                    child: cache_child,
                    log,
                }],
                ingest: ingest_child,
                pid: cache_pid,
                cache_dir: Some(cache_dir.clone()),
            };
            *supervisor = Some(process);

            let reader_playlist_path = playlist_path.clone();
            let reader_ffmpeg_cmd = ffmpeg_cmd.clone();
            let reader_log_level = log_level.clone();
            let reader_crop = crop;
            let reader_rtmp_url_key = rtmp_url_key.clone();
            let reader_context = context.clone();
            let reader_task = tokio::spawn(async move {
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
                        "{reader_context} 缓存预缓冲失败；等待{}仍无播放列表，准备停止会话",
                        human_duration((latency_secs + CACHE_PLAYLIST_WAIT_SECS) as i64)
                    );
                    stop_ffmpeg_session(session).await;
                    return;
                }

                let mut reader_cmd = Command::new(reader_ffmpeg_cmd);
                configure_tokio_no_window(&mut reader_cmd);

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

                append_flv_output_options(&mut reader_cmd);
                reader_cmd
                    .arg(reader_rtmp_url_key)
                    .kill_on_drop(true)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());

                let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
                let Some(process) = matching_process(&mut supervisor, session) else {
                    return;
                };
                match FFMPEG_SUPERVISOR.spawn(&mut reader_cmd) {
                    Ok(mut reader_child) => {
                        let reader_pid = reader_child.id();
                        let log = ChildLogContext::new(
                            &reader_context,
                            ChildRole::Ffmpeg(FfmpegStatsRole::Push),
                            reader_pid,
                        );
                        log.started();
                        if let Some(pid_value) = reader_pid {
                            set_high_priority(pid_value);
                        }
                        mark_push_process_started();
                        if let Some(stderr) = reader_child.stderr.take() {
                            process.tasks.0.push(spawn_ffmpeg_stderr_monitor(
                                stderr,
                                reader_log_level,
                                log.clone(),
                            ));
                        }

                        process.children.push(FfmpegChild {
                            child: reader_child,
                            log,
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            "{reader_context} 延迟推流进程启动失败；{}",
                            safe_diagnostic(&e.to_string())
                        );
                        drop(supervisor);
                        stop_ffmpeg_session(session).await;
                    }
                }
            });

            if let Some(process) = supervisor.as_mut() {
                process.tasks.0.push(reader_task);
                process.tasks.0.push(start_ffmpeg_timeout_monitor(
                    session,
                    cache_startup_timeout_secs(latency_secs),
                ));
            }
        }
        Err(e) => {
            tracing::error!(
                "{context} 缓存进程启动失败；{}",
                safe_diagnostic(&e.to_string())
            );
            if let Some(mut ingest) = ingest_child {
                kill_child_tree(&mut ingest).await;
            }
        }
    }
}

/// Spawns and supervises an ffmpeg process with output monitoring
pub async fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    source: FfmpegSource,
    proxy: Option<String>,
    log_level: String,
    crop: Option<(u32, u32, u32, u32)>, // (width, height, x, y)
    cache: FfmpegCacheOptions,
    identity: FfmpegStreamIdentity,
) -> Option<FfmpegSession> {
    FFMPEG_SUPERVISOR
        .interruptible(async {
            // Serialize the running check, process spawn, and registration with stops.
            let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
            if supervisor.is_some() {
                return None;
            }

            // Recheck cluster execution while holding the same lock as demotion's stop.
            // A task queued on this lock before a handoff must not start afterwards.
            let cfg = match crate::config::load_config()
                .await
                .map_err(|e| e.to_string())
            {
                Ok(cfg) => cfg,
                Err(error) => {
                    tracing::warn!("{identity} 启动已取消；{}", safe_diagnostic(&error));
                    return None;
                }
            };
            if !crate::cluster::local_monitoring_allowed(&cfg) {
                return None;
            }
            let rtmp_url_key = format!("{}{}", rtmp_url, rtmp_key);
            let latency_secs = cache.latency_secs();

            if cache.enabled {
                spawn_cached_ffmpeg(
                    &mut supervisor,
                    rtmp_url_key,
                    source,
                    proxy,
                    log_level,
                    crop,
                    latency_secs,
                    identity,
                )
                .await;
            } else {
                spawn_direct_ffmpeg(
                    &mut supervisor,
                    rtmp_url_key,
                    source,
                    proxy,
                    log_level,
                    crop,
                    identity,
                )
                .await;
            }
            supervisor.as_ref().map(|process| process.session)
        })
        .await
        .flatten()
}

/// Wait for the ffmpeg process to exit and return the exit status
/// This function blocks until ffmpeg exits or is killed
pub async fn wait_ffmpeg() -> Option<std::process::ExitStatus> {
    let session = current_ffmpeg_session().await?;
    wait_ffmpeg_session(session).await
}

pub async fn wait_ffmpeg_session(session: FfmpegSession) -> Option<std::process::ExitStatus> {
    // Poll to check if process is still running, allowing stop_ffmpeg to interrupt
    loop {
        let mut supervisor = FFMPEG_SUPERVISOR.lock().await;

        if let Some(process) = matching_process(&mut supervisor, session) {
            // Check if process has exited without blocking
            for child in &mut process.children {
                match child.child.try_wait() {
                    Ok(Some(status)) => {
                        child.log.exited(&status);

                        drop(supervisor);
                        stop_ffmpeg_session(session).await;
                        return Some(status);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(
                            "{} 检查进程状态失败 pid={}；{}",
                            child.log,
                            child.log.pid_label(),
                            safe_diagnostic(&e.to_string())
                        );
                        drop(supervisor);
                        stop_ffmpeg_session(session).await;
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

fn log_stuck(context: &SessionLogContext, reason: StuckReason) {
    match reason {
        StuckReason::NoStats {
            elapsed_secs,
            source,
        } => {
            tracing::error!(
                "{context} {source} 停滞，准备终止进程；{}无 stats 输出",
                human_duration(elapsed_secs as i64)
            );
        }
        StuckReason::StreamTimeFrozen { elapsed_secs } => {
            tracing::error!(
                "{context} 推流停滞，准备终止进程；流时间冻结{}",
                human_duration(elapsed_secs as i64)
            );
        }
        StuckReason::LowSpeed { elapsed_secs } => {
            tracing::error!(
                "{context} 推流停滞，准备终止进程；速度低于{}持续{}",
                LOW_SPEED_THRESHOLD,
                human_duration(elapsed_secs as i64)
            );
        }
        StuckReason::IdleDrain { elapsed_secs } => {
            tracing::info!(
                "{context} 推流排空，准备停止进程；{}无数据",
                human_duration(elapsed_secs as i64)
            );
        }
    }
}

/// Background task to monitor ffmpeg timeout and kill if stuck
async fn monitor_ffmpeg_timeout(session: FfmpegSession, timeout_secs: u64) {
    loop {
        let mut supervisor = FFMPEG_SUPERVISOR.lock().await;
        let Some(process) = matching_process(&mut supervisor, session) else {
            // Process exited, stop monitoring
            break;
        };
        let context = process.log.clone();

        let stuck = check_ffmpeg_stuck(timeout_secs);
        drop(supervisor);
        if let Some(reason) = stuck {
            log_stuck(&context, reason);
            stop_ffmpeg_session(session).await;
            break;
        }

        // Check every 5 seconds
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    #[derive(Clone, Default)]
    struct TestLogWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for TestLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs(run: impl FnOnce()) -> String {
        let output = TestLogWriter::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = output.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    struct TestStderr {
        chunks: VecDeque<Vec<u8>>,
        offset: usize,
        yield_next: bool,
        error: Option<std::io::Error>,
    }

    impl TestStderr {
        fn new(bytes: &[u8], chunk_size: usize) -> Self {
            Self {
                chunks: bytes.chunks(chunk_size).map(<[u8]>::to_vec).collect(),
                offset: 0,
                yield_next: false,
                error: None,
            }
        }
    }

    impl AsyncRead for TestStderr {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.yield_next {
                self.yield_next = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if let Some(chunk) = self.chunks.front() {
                let count = output.remaining().min(chunk.len() - self.offset);
                output.put_slice(&chunk[self.offset..self.offset + count]);
                let complete = self.offset + count == chunk.len();
                self.offset += count;
                if complete {
                    self.chunks.pop_front();
                    self.offset = 0;
                }
                self.yield_next = true;
                Poll::Ready(Ok(()))
            } else if let Some(error) = self.error.take() {
                Poll::Ready(Err(error))
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    fn reader_context(session: FfmpegSession, role: ChildRole) -> ChildLogContext {
        ChildLogContext::new(&test_context(session), role, Some(12345))
    }

    fn test_context(session: FfmpegSession) -> SessionLogContext {
        SessionLogContext::new(
            FfmpegStreamIdentity::new("jp", "YT", "示例频道001", "example-channel-001"),
            session,
        )
    }

    fn test_child(child: Child, session: FfmpegSession) -> FfmpegChild {
        FfmpegChild {
            log: ChildLogContext::new(
                &test_context(session),
                ChildRole::Ffmpeg(FfmpegStatsRole::Push),
                child.id(),
            ),
            child,
        }
    }

    fn empty_process(session: FfmpegSession) -> FfmpegProcess {
        FfmpegProcess {
            session,
            log: test_context(session),
            tasks: SessionTasks::default(),
            children: Vec::new(),
            ingest: None,
            pid: None,
            cache_dir: None,
        }
    }

    #[test]
    fn stderr_reader_preserves_split_utf8_delimiters_and_final_diagnostic() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let session = FfmpegSession(41);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                let bytes = concat!(
                "ERROR 示例诊断 rtmp://example.invalid/live/SYNTHETIC_RTMP?key=SYNTHETIC_KEY Broken pipe\r\n",
                "WARNING Cookie: SESSDATA=SYNTHETIC_COOKIE\r",
                "Conversion failed!"
            );
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(bytes.as_bytes(), 1),
                    "debug",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Push)),
                )
                .await;
            })
        });
        assert_eq!(output.lines().count(), 3, "{output}");
        assert!(output.contains("ERROR jp YT/示例频道001 s=41 推流"));
        assert!(output.contains("示例诊断"));
        assert_eq!(output.matches("Broken pipe").count(), 1);
        assert_eq!(output.matches("Conversion failed").count(), 1);
        assert!(output.contains("WARN jp"));
        assert!(!output.contains('�'));
        assert!(!output.contains("SYNTHETIC_"));
        assert!(!output.contains("example.invalid"));
    }

    #[test]
    fn stderr_reader_flushes_fragment_and_reports_read_error_once() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let session = FfmpegSession(42);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                let mut stderr = TestStderr::new(b"Broken pipe", 2);
                stderr.error = Some(std::io::Error::other(
                "HTTP 502 https://SYNTHETIC_USER:SYNTHETIC_PASSWORD@proxy.invalid/read token=SYNTHETIC_TOKEN",
            ));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    stderr,
                    "error",
                    reader_context(session, ChildRole::Ingest),
                )
                .await;
            })
        });
        assert_eq!(output.lines().count(), 2, "{output}");
        assert_eq!(output.matches("Broken pipe").count(), 1);
        assert_eq!(output.matches("读取诊断失败").count(), 1);
        assert!(output.contains("s=42 摄取"));
        assert!(output.contains("HTTP 502"));
        assert!(!output.contains("SYNTHETIC_"));
        assert!(!output.contains("proxy.invalid"));
    }

    #[test]
    fn oversized_stderr_discards_secret_tail_through_the_delimiter() {
        let mut diagnostic = b"ERROR rtmp://example.invalid/live/".to_vec();
        diagnostic.resize(STDERR_LINE_LIMIT + 20, b'x');
        diagnostic.extend_from_slice(
            b"SYNTHETIC_UNLABELLED_TAIL\nWARNING next diagnostic\nConversion failed!",
        );
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let session = FfmpegSession(43);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(&diagnostic, 8191),
                    "debug",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Cache)),
                )
                .await;
            })
        });
        assert_eq!(output.lines().count(), 3, "{output}");
        assert_eq!(output.matches("原始行超过64 KiB，已截断").count(), 1);
        assert!(output.contains("[诊断续行已省略]"));
        assert!(output.contains("Conversion failed"));
        assert!(output.lines().all(|line| line.len() < 600));
        assert!(!output.contains("SYNTHETIC_UNLABELLED_TAIL"));
        assert!(!output.contains("example.invalid"));
    }

    #[test]
    fn interleaved_children_and_replacement_keep_their_owned_identity() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let session = FfmpegSession(44);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                let cache = monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(b"HTTP error 403 Forbidden\nBroken pipe\n", 4),
                    "error",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Cache)),
                );
                let push = monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(b"Broken pipe\nERROR writing trailer: Broken pipe\n", 4),
                    "error",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Push)),
                );
                tokio::join!(cache, push);

                let replacement = FfmpegSession(45);
                let identity =
                    FfmpegStreamIdentity::new("ca", "TW", "示例频道002", "example-channel-002");
                let context = SessionLogContext::new(identity, replacement);
                *supervisor.lock().await = Some(FfmpegProcess {
                    log: context.clone(),
                    ..empty_process(replacement)
                });
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(b"Broken pipe", 4),
                    "error",
                    ChildLogContext::new(
                        &context,
                        ChildRole::Ffmpeg(FfmpegStatsRole::Push),
                        Some(54321),
                    ),
                )
                .await;
            })
        });
        assert!(output.contains("jp YT/示例频道001 s=44 缓存；HTTP error 403 Forbidden"));
        assert!(output.contains("jp YT/示例频道001 s=44 缓存；Broken pipe"));
        assert!(output.contains("jp YT/示例频道001 s=44 推流；Broken pipe"));
        assert!(output.contains("ca TW/示例频道002 s=45 推流；Broken pipe"));
        assert!(output.contains("已合并1条重复诊断"));
        assert_eq!(output.lines().count(), 5, "{output}");
    }

    #[test]
    fn wrapped_native_credentials_stay_hidden_until_the_next_stats_boundary() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                reset_ffmpeg_tracking_state();
                let session = FfmpegSession(55);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(
                        concat!(
                            "ERROR opening rtmp://example.invalid/live/\r\n",
                            "SYNTHETIC_UNLABELLED_CREDENTIAL\n",
                            "Server returned 403 Forbidden\n",
                            "Broken pipe\n",
                            "frame=0 fps=0 time=00:00:00.00 bitrate=0kbits/s speed=0x\r",
                            "WARNING new independent diagnostic"
                        )
                        .as_bytes(),
                        3,
                    ),
                    "debug",
                    reader_context(session, ChildRole::Ingest),
                )
                .await;
                reset_stopped_tracking_state();
            })
        });
        assert_eq!(output.lines().count(), 5, "{output}");
        assert!(output.contains("[诊断续行已省略]"));
        assert!(output.contains("HTTP 403"), "{output}");
        assert!(output.contains("Broken pipe"));
        assert!(output.contains("WARNING new independent diagnostic"));
        assert!(!output.contains("SYNTHETIC_"));
        assert!(!output.contains("example.invalid"));
    }

    #[test]
    fn sensitive_or_truncated_stats_keep_wrapped_tails_private() {
        let mut diagnostic = concat!(
            "frame=1 fps=1 time=00:00:01.00 bitrate=0kbits/s speed=0x token=SYNTHETIC_STATS_TOKEN\n",
            "SYNTHETIC_STATS_TOKEN_TAIL\n",
            "frame=2 fps=2 time=00:00:02.00 bitrate=0kbits/s speed=0x\r",
            "WARNING independent diagnostic\n"
        )
        .as_bytes()
        .to_vec();
        let oversized_start = diagnostic.len();
        diagnostic.extend_from_slice(b"frame=3 fps=3 time=00:00:03.00 bitrate=0kbits/s speed=0x ");
        diagnostic.resize(oversized_start + STDERR_LINE_LIMIT + 5, b'x');
        diagnostic.extend_from_slice(b"\nSYNTHETIC_TRUNCATED_STATS_TAIL\n");
        let output = capture_logs(|| {
            run_supervisor_test(async {
                reset_ffmpeg_tracking_state();
                let session = FfmpegSession(57);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(&diagnostic, 8191),
                    "debug",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Push)),
                )
                .await;
                assert_eq!(FFMPEG_PUSH_FRAME.load(Ordering::Relaxed), 3);
                assert_eq!(optional_f32_from_atomic_bits(&FFMPEG_PUSH_FPS), Some(3.0));
                assert_eq!(LAST_STREAM_TIME.load(Ordering::Relaxed), 3);
                assert!(LAST_PUSH_PROGRESS_TIME.load(Ordering::Relaxed) > 0);
                assert_eq!(LAST_CACHE_PROGRESS_TIME.load(Ordering::Relaxed), 0);
                reset_stopped_tracking_state();
            })
        });
        assert!(
            output.contains("WARNING independent diagnostic"),
            "{output}"
        );
        assert!(!output.contains("SYNTHETIC_"), "{output}");
        assert!(!output.contains("已观察到推流"), "{output}");
    }

    #[test]
    fn native_warning_then_error_preserves_severity_and_new_failure() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let session = FfmpegSession(58);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(
                        concat!(
                            "WARNING Connection reset; retrying\n",
                            "Connection reset by peer\n",
                            "[HLS] KEEPALIVE request failed: Input/output ERROR\n",
                            "Permission denied\n",
                            "End of file\n",
                            "No space left on device\n",
                            "Protocol not found\n"
                        )
                        .as_bytes(),
                        7,
                    ),
                    "error",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Cache)),
                )
                .await;
            })
        });
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 7, "{output}");
        assert!(lines[0].trim_start().starts_with("WARN "), "{output}");
        assert!(lines[1].starts_with("ERROR "), "{output}");
        assert!(lines[1].contains("Connection reset by peer"), "{output}");
        assert!(lines[2].starts_with("DEBUG "), "{output}");
        assert!(
            lines[3..].iter().all(|line| line.starts_with("ERROR ")),
            "{output}"
        );
        assert!(!output.contains("已合并"), "{output}");
    }

    #[test]
    fn repeated_child_error_classes_have_bounded_summaries_and_terminal_output() {
        let output = capture_logs(|| {
            let mut reporter =
                StderrReporter::new(reader_context(FfmpegSession(46), ChildRole::Ingest));
            let now = reporter.log.started;
            reporter.diagnostic("Broken pipe", "debug", false, now);
            for index in 1..=20 {
                reporter.diagnostic(
                    "ERROR writing trailer: Broken pipe",
                    "debug",
                    false,
                    now + Duration::from_secs(index * 15),
                );
            }
            reporter.diagnostic(
                "Broken pipe",
                "debug",
                false,
                now + Duration::from_secs(301),
            );
            reporter.diagnostic(
                "ERROR new decoder failure",
                "debug",
                false,
                now + Duration::from_secs(302),
            );
            reporter.diagnostic(
                "[hls] keepalive request failed: Connection reset",
                "debug",
                false,
                now + Duration::from_secs(303),
            );
            reporter.repeat_summary("诊断读取结束", now + Duration::from_secs(304));
        });
        assert_eq!(
            output
                .lines()
                .filter(|line| line.contains("Broken pipe"))
                .count(),
            3,
            "{output}"
        );
        assert!(output.contains("已合并20条重复诊断"));
        assert!(output.contains("诊断读取结束；已合并1条重复诊断"));
        assert!(output.contains("ERROR jp YT/示例频道001 s=46 摄取；ERROR new decoder failure"));
        assert!(output
            .lines()
            .any(|line| line.starts_with("DEBUG") && line.contains("keepalive request failed")));
    }

    struct PausedStderr {
        fragment: Option<&'static [u8]>,
        waiting: Option<tokio::sync::oneshot::Sender<()>>,
        release: tokio::sync::oneshot::Receiver<()>,
    }

    impl AsyncRead for PausedStderr {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(fragment) = self.fragment.take() {
                output.put_slice(fragment);
                return Poll::Ready(Ok(()));
            }
            if let Some(waiting) = self.waiting.take() {
                let _ = waiting.send(());
            }
            Pin::new(&mut self.release).poll(cx).map(|_| Ok(()))
        }
    }

    #[test]
    fn stale_reader_eof_never_flushes_into_a_replacement_session() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let supervisor = Arc::new(ProcessSupervisor::default());
                let old = FfmpegSession(47);
                *supervisor.lock().await = Some(empty_process(old));
                let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let reader = tokio::spawn({
                    let supervisor = supervisor.clone();
                    async move {
                        monitor_ffmpeg_stderr(
                            &supervisor,
                            PausedStderr {
                                fragment: Some(b"ERROR stale unterminated diagnostic"),
                                waiting: Some(waiting_tx),
                                release: release_rx,
                            },
                            "debug",
                            reader_context(old, ChildRole::Ingest),
                        )
                        .await;
                    }
                });
                waiting_rx.await.unwrap();
                *supervisor.lock().await = Some(empty_process(FfmpegSession(48)));
                release_tx.send(()).unwrap();
                reader.await.unwrap();
                assert_eq!(
                    supervisor.lock().await.as_ref().unwrap().session,
                    FfmpegSession(48)
                );
            })
        });
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn aborting_an_owned_reader_does_not_drain_or_emit_its_partial_diagnostic() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                let supervisor = Arc::new(ProcessSupervisor::default());
                let session = FfmpegSession(49);
                let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
                let mut process = empty_process(session);
                process.tasks.0.push(tokio::spawn({
                    let supervisor = supervisor.clone();
                    async move {
                        let _on_drop = dropped_tx;
                        monitor_ffmpeg_stderr(
                            &supervisor,
                            PausedStderr {
                                fragment: Some(b"Broken pipe from an aborted reader"),
                                waiting: Some(waiting_tx),
                                release: release_rx,
                            },
                            "debug",
                            reader_context(session, ChildRole::Ingest),
                        )
                        .await;
                    }
                }));
                *supervisor.lock().await = Some(process);
                waiting_rx.await.unwrap();
                *supervisor.lock().await = Some(empty_process(FfmpegSession(50)));
                assert!(tokio::time::timeout(Duration::from_secs(1), dropped_rx)
                    .await
                    .unwrap()
                    .is_err());
                assert!(
                    release_tx.send(()).is_err(),
                    "abort must drop the blocked reader"
                );
            })
        });
        assert!(output.is_empty(), "{output}");
    }

    #[test]
    fn reader_stats_preserve_meters_and_only_observed_push_reports_publishing() {
        let session = FfmpegSession(51);
        let push_log = reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Push));
        let startup = capture_logs(|| push_log.started());
        assert!(startup.contains("进程已启动 pid=12345"));
        assert!(!startup.contains("已观察到推流"));

        let output = capture_logs(|| {
            run_supervisor_test(async {
                reset_ffmpeg_tracking_state();
                FFMPEG_CACHE_BITRATE_KBPS.store(910.0_f32.to_bits(), Ordering::Relaxed);
                let supervisor = ProcessSupervisor::default();
                *supervisor.lock().await = Some(empty_process(session));
                monitor_ffmpeg_stderr(
                    &supervisor,
                    TestStderr::new(
                        b"frame=60 fps=60 time=00:00:04.00 bitrate=9999kbits/s speed=1.5x\r",
                        5,
                    ),
                    "debug",
                    reader_context(session, ChildRole::Ffmpeg(FfmpegStatsRole::Cache)),
                )
                .await;
                assert_eq!(f32_from_atomic_bits(&FFMPEG_CACHE_SPEED), Some(1.5));
                assert_eq!(
                    f32_from_atomic_bits(&FFMPEG_CACHE_BITRATE_KBPS),
                    Some(910.0)
                );
                assert_eq!(FFMPEG_CACHE_TIME_SECS.load(Ordering::Relaxed), 4);
                assert!(LAST_CACHE_PROGRESS_TIME.load(Ordering::Relaxed) > 0);
                assert_eq!(LAST_PUSH_PROGRESS_TIME.load(Ordering::Relaxed), 0);
                assert_eq!(LAST_STREAM_TIME.load(Ordering::Relaxed), 0);

                monitor_ffmpeg_stderr(&supervisor,
                TestStderr::new(concat!(
                    "frame=0 fps=0 time=00:00:00.00 bitrate=0kbits/s speed=0x\r\n",
                    "frame=120 fps=60 time=00:00:02.00 bitrate=6000kbits/s speed=1x ERROR token=SYNTHETIC_STATS_TOKEN\r",
                    "frame=180 fps=60 size=96KiB time=00:00:03.00 bitrate=4000kbits/s speed=1x"
                ).as_bytes(), 7), "debug", push_log,
            ).await;
                assert_eq!(f32_from_atomic_bits(&FFMPEG_SPEED), Some(1.0));
                assert_eq!(f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS), Some(6_000.0));
                assert_eq!(FFMPEG_TOTAL_BYTES.load(Ordering::Relaxed), 96 * 1024);
                assert_eq!(FFMPEG_PUSH_FRAME.load(Ordering::Relaxed), 180);
                assert_eq!(optional_f32_from_atomic_bits(&FFMPEG_PUSH_FPS), Some(60.0));
                assert_eq!(LAST_STREAM_TIME.load(Ordering::Relaxed), 3);
                assert!(LAST_STREAM_TIME_UPDATE.load(Ordering::Relaxed) > 0);
                assert!(LAST_PUSH_PROGRESS_TIME.load(Ordering::Relaxed) > 0);
                assert_eq!(LOW_SPEED_SINCE.load(Ordering::Relaxed), 0);
                reset_stopped_tracking_state();
            })
        });
        assert_eq!(output.lines().count(), 1, "{output}");
        assert!(output.contains("s=51 推流 已观察到推流输出"));
        assert!(!output.contains("SYNTHETIC_STATS_TOKEN"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actual_child_exit_and_manual_stop_have_numeric_truthful_outcomes() {
        let output = capture_logs(|| {
            run_supervisor_test(async {
                clear_manual_stop();
                let session = FfmpegSession(52);
                let mut command = Command::new("sh");
                command.args(["-c", "exit 7"]).kill_on_drop(true);
                let child = FFMPEG_SUPERVISOR.spawn(&mut command).unwrap();
                let log = ChildLogContext::new(
                    &test_context(session),
                    ChildRole::Ffmpeg(FfmpegStatsRole::Cache),
                    child.id(),
                );
                log.started();
                *FFMPEG_SUPERVISOR.lock().await = Some(FfmpegProcess {
                    children: vec![FfmpegChild { child, log }],
                    ..empty_process(session)
                });
                assert_eq!(wait_ffmpeg_session(session).await.unwrap().code(), Some(7));

                let session = FfmpegSession(53);
                let child = FFMPEG_SUPERVISOR.spawn(&mut sleeper()).unwrap();
                let child = test_child(child, session);
                child.log.started();
                *FFMPEG_SUPERVISOR.lock().await = Some(FfmpegProcess {
                    children: vec![child],
                    ..empty_process(session)
                });
                stop_ffmpeg().await;
                assert!(FFMPEG_SUPERVISOR.lock().await.is_none());
                assert!(was_manual_stop());
                clear_manual_stop();
            })
        });
        assert!(output.contains("s=52 缓存 进程已启动 pid="));
        assert_eq!(output.matches("进程异常退出").count(), 1);
        assert_eq!(output.matches("exit=7").count(), 1);
        assert!(output.contains("s=53 ffmpeg 子进程已停止；原因=主动停止请求"));
        assert!(!output.contains("Some("));
        assert!(!output.contains("None"));
        assert!(!output.contains("ExitStatus("));
    }

    #[test]
    fn stalls_and_idle_drain_keep_role_duration_and_severity() {
        let output = capture_logs(|| {
            let context = test_context(FfmpegSession(54));
            log_stuck(
                &context,
                StuckReason::NoStats {
                    elapsed_secs: 31,
                    source: "HLS 缓存写入",
                },
            );
            log_stuck(&context, StuckReason::StreamTimeFrozen { elapsed_secs: 12 });
            log_stuck(&context, StuckReason::LowSpeed { elapsed_secs: 65 });
            log_stuck(&context, StuckReason::IdleDrain { elapsed_secs: 10 });
        });
        let records: Vec<_> = output.lines().collect();
        assert_eq!(records.len(), 4);
        assert!(records[..3].iter().all(|line| line.starts_with("ERROR")));
        assert!(records[0].contains("s=54 HLS 缓存写入 停滞"));
        assert!(records[0].contains("31秒无 stats"));
        assert!(records[1].contains("推流停滞"));
        assert!(records[2].contains("1分5秒"));
        assert!(records[3].trim_start().starts_with("INFO"));
        assert!(records[3].contains("推流排空"));
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_pid_and_signal_exit_have_explicit_human_values() {
        use std::os::unix::process::ExitStatusExt;
        let output = capture_logs(|| {
            let context =
                ChildLogContext::new(&test_context(FfmpegSession(56)), ChildRole::Ingest, None);
            context.started();
            context.exited(&std::process::ExitStatus::from_raw(15));
        });
        assert!(output.contains("s=56 摄取 进程已启动 pid=未知"));
        assert!(output.contains("进程异常退出 pid=未知 signal=15"));
        assert!(!output.contains("Some("));
        assert!(!output.contains("None"));
        assert!(!output.contains("ExitStatus("));
    }

    #[tokio::test]
    async fn replacing_session_cancels_its_sleeping_workers() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let mut old = empty_process(FfmpegSession(1));
        old.tasks.0.push(tokio::spawn(async move {
            let _on_drop = dropped_tx;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started_rx.await.unwrap();
        let mut supervisor = Some(old);
        let _ = supervisor.replace(empty_process(FfmpegSession(2)));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
                .await
                .unwrap()
                .is_err()
        );
        assert!(matching_process(&mut supervisor, FfmpegSession(1)).is_none());
        assert_eq!(supervisor.as_ref().unwrap().session, FfmpegSession(2));
    }

    #[test]
    fn stale_session_cannot_register_a_worker_for_replacement() {
        run_supervisor_test(async {
            *FFMPEG_SUPERVISOR.lock().await = Some(empty_process(FfmpegSession(2)));
            assert!(
                crate::webui::api::current_network_status()
                    .await
                    .ffmpeg_running
            );
            let called = Arc::new(AtomicBool::new(false));
            let worker_called = called.clone();
            assert!(!stop_ffmpeg_session(FfmpegSession(1)).await);
            let stale_called = called.clone();
            assert!(
                !transition_ffmpeg_session(FfmpegSession(1), async move {
                    stale_called.store(true, Ordering::SeqCst);
                    true
                })
                .await
            );
            assert!(!transition_ffmpeg_session(FfmpegSession(2), async { false }).await);
            spawn_session_task(FfmpegSession(1), async move {
                worker_called.store(true, Ordering::SeqCst);
            })
            .await;
            tokio::task::yield_now().await;
            assert!(!called.load(Ordering::SeqCst));
            let process = FFMPEG_SUPERVISOR.lock().await.take().unwrap();
            assert_eq!(process.session, FfmpegSession(2));
            assert!(process.tasks.0.is_empty());
            // A remote publisher can keep the room live after this local session
            // ends. The status API must still advertise an idle local publisher.
            let network = crate::webui::api::current_network_status().await;
            assert!(!network.ffmpeg_running);
            assert!(network.stream_bitrate_history.is_empty());
        });
    }

    lazy_static::lazy_static! {
        static ref FFMPEG_COUNTER_TEST_LOCK: StdMutex<()> = StdMutex::new(());
    }

    #[cfg(target_os = "linux")]
    async fn wait_exited(pid: u32) -> bool {
        for _ in 0..100 {
            // A zombie no longer publishes; only its parent's reaper is pending.
            let exited =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                    stat.rsplit_once(')')
                        .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
                });
            if exited {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// Fencing resets the shared counters, and the process supervisor is global,
    /// so these tests hold the counter lock for the whole runtime.
    fn run_supervisor_test(test: impl Future<Output = ()>) {
        let _guard = FFMPEG_COUNTER_TEST_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(test);
    }

    #[test]
    fn fence_finishes_teardown_and_releases_locks_before_writing_logs() {
        use tracing::instrument::WithSubscriber;

        let supervisor = Arc::new(ProcessSupervisor::default());
        let log_supervisor = Arc::clone(&supervisor);
        let writes = Arc::new(AtomicU64::new(0));
        let log_writes = Arc::clone(&writes);
        let released = Arc::new(AtomicBool::new(true));
        let log_released = Arc::clone(&released);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || {
                let stopped = log_supervisor
                    .process
                    .try_lock()
                    .is_ok_and(|process| process.is_none());
                let unlocked = log_supervisor.fence_lock.try_lock().is_ok();
                log_released.fetch_and(
                    stopped && unlocked && !*log_supervisor.fencing.borrow(),
                    Ordering::SeqCst,
                );
                log_writes.fetch_add(1, Ordering::SeqCst);
                std::io::sink()
            })
            .finish();
        run_supervisor_test(async {
            // Another test can reach this callsite without a subscriber first.
            let earlier = ProcessSupervisor::default();
            *earlier.lock().await = Some(empty_process(FfmpegSession(58)));
            earlier
                .fence()
                .with_subscriber(tracing::Dispatch::none())
                .await;

            // Register only after acquiring the shared test lock. Otherwise an
            // unscoped fence can cache Interest::never while this test waits,
            // and entering our already-registered subscriber will not rebuild it.
            async {
                *supervisor.lock().await = Some(empty_process(FfmpegSession(59)));
                supervisor.fence().await;
            }
            .with_subscriber(subscriber)
            .await;
        });
        assert!(writes.load(Ordering::SeqCst) > 0);
        assert!(released.load(Ordering::SeqCst));
    }

    #[cfg(target_os = "linux")]
    fn run_fence_test(test: impl Future<Output = ()>) {
        run_supervisor_test(test);
    }

    #[cfg(target_os = "linux")]
    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command.arg("1000").kill_on_drop(true);
        command
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fence_kills_every_publisher_while_preparation_holds_the_supervisor() {
        use tokio::io::AsyncBufReadExt;
        run_fence_test(async {
            let supervisor = Arc::new(ProcessSupervisor::default());
            let publisher = supervisor.spawn(&mut sleeper()).unwrap();
            let mut ingest = Command::new("sh");
            configure_ingest_process_group(&mut ingest);
            ingest
                .args(["-c", "sleep 1000 & echo $!; wait"])
                .kill_on_drop(true)
                .stdout(Stdio::piped());
            let mut ingest = supervisor.spawn(&mut ingest).unwrap();
            let mut muxer = String::new();
            tokio::io::BufReader::new(ingest.stdout.take().unwrap())
                .read_line(&mut muxer)
                .await
                .unwrap();
            let pids = [
                publisher.id().unwrap(),
                ingest.id().unwrap(),
                muxer.trim().parse().unwrap(),
            ];
            let session = FfmpegSession(u64::MAX);
            *supervisor.lock().await = Some(FfmpegProcess {
                session,
                log: test_context(session),
                tasks: SessionTasks::default(),
                children: vec![test_child(publisher, session)],
                ingest: Some(IngestProcess(
                    ingest,
                    ChildLogContext::new(&test_context(session), ChildRole::Ingest, Some(pids[1])),
                )),
                pid: Some(pids[0]),
                cache_dir: None,
            });
            let (held_tx, held_rx) = tokio::sync::oneshot::channel();
            let preparing = tokio::spawn({
                let supervisor = supervisor.clone();
                async move {
                    supervisor
                        .transition(session, async move {
                            held_tx.send(()).unwrap();
                            std::future::pending::<bool>().await
                        })
                        .await
                }
            });
            held_rx.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.fence())
                .await
                .expect("fence must not wait for preparation holding the supervisor");
            assert!(!preparing.await.unwrap());
            assert!(supervisor.lock().await.is_none());
            for pid in pids {
                assert!(wait_exited(pid).await, "publisher {pid} survived the fence");
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fence_ffmpeg_kills_a_publisher_while_preparation_holds_the_process_supervisor() {
        run_fence_test(async {
            let publisher = FFMPEG_SUPERVISOR.spawn(&mut sleeper()).unwrap();
            let pid = publisher.id().unwrap();
            let session = FfmpegSession(u64::MAX);
            *FFMPEG_SUPERVISOR.lock().await = Some(FfmpegProcess {
                session,
                log: test_context(session),
                tasks: SessionTasks::default(),
                children: vec![test_child(publisher, session)],
                ingest: None,
                pid: Some(pid),
                cache_dir: None,
            });
            let (held_tx, held_rx) = tokio::sync::oneshot::channel();
            let preparing = tokio::spawn(async move {
                transition_ffmpeg_session(session, async move {
                    held_tx.send(()).unwrap();
                    std::future::pending::<bool>().await
                })
                .await
            });
            held_rx.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), fence_ffmpeg())
                .await
                .expect(
                    "fence_ffmpeg must not wait for preparation holding the process supervisor",
                );
            assert!(!preparing.await.unwrap());
            assert!(FFMPEG_SUPERVISOR.lock().await.is_none());
            assert!(
                wait_exited(pid).await,
                "publisher {pid} survived fence_ffmpeg"
            );
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fence_closes_the_unregistered_spawn_window() {
        run_fence_test(async {
            let supervisor = Arc::new(ProcessSupervisor::default());
            // Startup has spawned a child but not registered it yet.
            let (spawned_tx, spawned_rx) = tokio::sync::oneshot::channel();
            let startup = tokio::spawn({
                let supervisor = supervisor.clone();
                async move {
                    supervisor
                        .interruptible(async {
                            let _held = supervisor.lock().await;
                            let child = supervisor.spawn(&mut sleeper()).unwrap();
                            spawned_tx.send(child.id().unwrap()).unwrap();
                            std::future::pending::<()>().await;
                            drop(child);
                        })
                        .await
                }
            });
            let pid = spawned_rx.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.fence())
                .await
                .expect("fence must cancel startup holding the supervisor");
            assert!(startup.await.unwrap().is_none());
            assert!(wait_exited(pid).await, "unregistered child {pid} survived");

            // A worker already holding the lock (the delayed cache reader)
            // cannot admit a new publisher once the fence has begun.
            let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
            let registering = tokio::spawn({
                let supervisor = supervisor.clone();
                async move {
                    let _held = supervisor.lock().await;
                    locked_tx.send(()).unwrap();
                    while !*supervisor.fencing.borrow() {
                        tokio::task::yield_now().await;
                    }
                    supervisor.spawn(&mut sleeper()).map(|child| child.id())
                }
            });
            locked_rx.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.fence())
                .await
                .expect("fence must finish once the registering worker yields");
            let error = registering.await.unwrap().unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        });
    }

    #[test]
    fn network_idle_requires_both_paths_in_cache_mode() {
        assert!(!is_network_transfer_idle(true, false, 0.0, 0.0));
        assert!(!is_network_transfer_idle(true, true, 500.0, 0.0));
        assert!(!is_network_transfer_idle(true, true, 0.0, 500.0));
        assert!(is_network_transfer_idle(true, true, 0.0, 0.0));
    }

    #[test]
    fn network_idle_direct_mode_uses_push_only() {
        assert!(!is_network_transfer_idle(false, true, 500.0, 999.0));
        assert!(is_network_transfer_idle(false, true, 0.0, 999.0));
    }
    #[test]
    fn push_stats_update_preserves_previous_bitrate_when_sparse() {
        let mut display = FfmpegStatsDisplay::default();

        display.update(
            FfmpegStatsRole::Push,
            FfmpegStatsSample {
                frame: Some(60),
                fps: Some(60.0),
                time: Some("00:00:01.00".to_string()),
                bitrate: Some("6.00 Mb/s".to_string()),
                bitrate_kbps: Some(6_000.0),
                speed: Some(1.0),
                stream_time_secs: Some(1),
                ..FfmpegStatsSample::default()
            },
        );
        display.update(
            FfmpegStatsRole::Push,
            FfmpegStatsSample {
                frame: Some(120),
                fps: Some(60.0),
                time: Some("00:00:02.00".to_string()),
                speed: Some(1.0),
                stream_time_secs: Some(2),
                ..FfmpegStatsSample::default()
            },
        );

        let push = display.push.expect("push stats should be initialized");
        assert_eq!(push.frame, Some(120));
        assert_eq!(push.bitrate.as_deref(), Some("6.00 Mb/s"));
        assert_eq!(push.bitrate_kbps, Some(6_000.0));
    }

    #[test]
    fn network_history_is_sampled_without_a_tty() {
        let mut display = FfmpegStatsDisplay {
            enabled: false,
            ..FfmpegStatsDisplay::default()
        };

        display.update(
            FfmpegStatsRole::Push,
            FfmpegStatsSample {
                bitrate: Some("1.50 Mb/s".to_string()),
                bitrate_kbps: Some(1_500.0),
                time: Some("00:00:01.00".to_string()),
                stream_time_secs: Some(1),
                speed: Some(1.0),
                ..FfmpegStatsSample::default()
            },
        );

        assert_eq!(display.push_history, vec![1_500.0]);
        assert_eq!(display.cache_history, vec![0.0]);
        assert_eq!(display.rendered_lines, 0);
    }

    #[test]
    fn network_history_caps_at_limit() {
        let mut display = FfmpegStatsDisplay {
            enabled: false,
            ..FfmpegStatsDisplay::default()
        };

        for index in 0..(NETWORK_HISTORY_LIMIT + 5) {
            display.last_history_sample_ms = 0;
            display.update(
                FfmpegStatsRole::Push,
                FfmpegStatsSample {
                    bitrate_kbps: Some(index as f32),
                    time: Some("00:00:01.00".to_string()),
                    stream_time_secs: Some(1),
                    ..FfmpegStatsSample::default()
                },
            );
        }

        assert_eq!(display.push_history.len(), NETWORK_HISTORY_LIMIT);
        assert_eq!(display.push_history[0], 5.0);
        assert_eq!(
            display.push_history[NETWORK_HISTORY_LIMIT - 1],
            (NETWORK_HISTORY_LIMIT + 4) as f32
        );
    }

    #[test]
    fn total_byte_counter_ignores_non_monotonic_push_samples() {
        let _guard = FFMPEG_COUNTER_TEST_LOCK.lock().unwrap();
        reset_ffmpeg_tracking_state();
        FFMPEG_TOTAL_BYTES.store(2_000, Ordering::Relaxed);
        FFMPEG_LAST_SAMPLE_MS.store(now_millis().saturating_sub(1_000), Ordering::Relaxed);
        FFMPEG_BITRATE_KBPS.store(6_000.0_f32.to_bits(), Ordering::Relaxed);

        let bitrate = update_network_counters_from_total_bytes(FfmpegStatsRole::Push, 1_500);

        assert_eq!(bitrate, Some(6_000.0));
        assert_eq!(FFMPEG_TOTAL_BYTES.load(Ordering::Relaxed), 2_000);
        assert_eq!(f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS), Some(6_000.0));
    }

    #[test]
    fn total_byte_counter_ignores_too_fast_push_samples() {
        let _guard = FFMPEG_COUNTER_TEST_LOCK.lock().unwrap();
        reset_ffmpeg_tracking_state();
        FFMPEG_TOTAL_BYTES.store(1_000, Ordering::Relaxed);
        FFMPEG_LAST_SAMPLE_MS.store(now_millis(), Ordering::Relaxed);
        FFMPEG_BITRATE_KBPS.store(6_000.0_f32.to_bits(), Ordering::Relaxed);

        let bitrate = update_network_counters_from_total_bytes(FfmpegStatsRole::Push, 2_000);

        assert_eq!(bitrate, Some(6_000.0));
        assert_eq!(FFMPEG_TOTAL_BYTES.load(Ordering::Relaxed), 2_000);
        assert_eq!(f32_from_atomic_bits(&FFMPEG_BITRATE_KBPS), Some(6_000.0));
    }

    #[test]
    fn cache_stats_update_merges_sparse_samples() {
        let mut display = FfmpegStatsDisplay::default();

        display.update(
            FfmpegStatsRole::Cache,
            FfmpegStatsSample {
                time: Some("00:00:01.00".to_string()),
                stream_time_secs: Some(1),
                speed: Some(1.0),
                ..FfmpegStatsSample::default()
            },
        );
        display.update(
            FfmpegStatsRole::Cache,
            FfmpegStatsSample {
                bitrate: Some("2.00 Mbps".to_string()),
                bitrate_kbps: Some(2_000.0),
                ..FfmpegStatsSample::default()
            },
        );

        let cache = display.cache.expect("cache stats should be initialized");
        assert_eq!(cache.time.as_deref(), Some("00:00:01.00"));
        assert_eq!(cache.stream_time_secs, Some(1));
        assert_eq!(cache.speed, Some(1.0));
        assert_eq!(cache.bitrate.as_deref(), Some("2.00 Mbps"));
        assert_eq!(cache.bitrate_kbps, Some(2_000.0));
    }

    #[test]
    fn cache_bitrate_update_preserves_progress_fields() {
        let mut display = FfmpegStatsDisplay::default();
        display.update(
            FfmpegStatsRole::Cache,
            FfmpegStatsSample {
                time: Some("00:00:02.00".to_string()),
                stream_time_secs: Some(2),
                speed: Some(0.99),
                ..FfmpegStatsSample::default()
            },
        );

        display.update_cache_bitrate(1_500.0);

        let cache = display.cache.expect("cache stats should be initialized");
        assert_eq!(cache.time.as_deref(), Some("00:00:02.00"));
        assert_eq!(cache.stream_time_secs, Some(2));
        assert_eq!(cache.speed, Some(0.99));
        assert_eq!(cache.bitrate_kbps, Some(1_500.0));
    }

    #[test]
    fn cache_byte_tracker_records_growth_without_counting_shrink() {
        let mut tracker = CacheByteTracker::new(PathBuf::from("/tmp/cache-byte-tracker-test"));
        let path = PathBuf::from("segment.ts");

        assert_eq!(tracker.record_file_size(path.clone(), 100, 1), 100);
        assert_eq!(tracker.record_file_size(path.clone(), 175, 2), 75);
        assert_eq!(tracker.record_file_size(path, 90, 3), 0);
    }

    #[test]
    fn cache_byte_tracker_prunes_files_missing_from_current_scan() {
        let mut tracker = CacheByteTracker::new(PathBuf::from("/tmp/cache-byte-tracker-test"));
        let keep = PathBuf::from("keep.ts");
        let remove = PathBuf::from("remove.ts");

        tracker.record_file_size(keep.clone(), 100, 1);
        tracker.record_file_size(remove.clone(), 100, 1);
        tracker.record_file_size(keep.clone(), 120, 2);
        tracker.prune_unseen_files(2);

        assert!(tracker.file_sizes.contains_key(&keep));
        assert!(!tracker.file_sizes.contains_key(&remove));
    }
}
