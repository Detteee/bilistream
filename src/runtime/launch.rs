//! CLI options, shared application lifecycle, and log capture.

use super::{graceful_shutdown, run_bilistream};
use crate as bilistream;
use crate::logging::{strip_ansi_codes, HumanFormat};
use crate::plugins::stop_danmaku;
use std::time::Duration;
use tracing_subscriber::fmt;

#[derive(Debug)]
struct LaunchArgs {
    bind: String,
    password: Option<String>,
    password_file: Option<std::path::PathBuf>,
    restart_password_file: Option<std::path::PathBuf>,
    port: u16,
    ffmpeg_log_level: String,
    tray: bool,
}

#[derive(Debug)]
enum ParseOutcome {
    Launch(LaunchArgs),
    ExportLegacy(std::path::PathBuf),
    ResetPanelPassword,
    Help,
    Version,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_tray() -> bool {
    cfg!(target_os = "windows")
}

fn print_help() {
    println!(
        "bilistream {}\n\n\
Start the Web UI and stream monitor. Setup and controls are in the browser.\n\n\
Usage: bilistream [OPTIONS]\n\n\
Options:\n\
  --bind ADDR                 Listen address (default 127.0.0.1, or BILISTREAM_BIND)\n\
  -p, --port PORT             Web UI port (default 3150, or BILISTREAM_PORT)\n\
  --password PASSWORD         Bootstrap panel password (or BILISTREAM_PASSWORD)\n\
  --password-file PATH        Read password from a private file\n\
  --ffmpeg-log-level LEVEL    error, info, or debug (default error)\n\
  --tray                      System tray (default on Windows)\n\
  --webui                     Console Web UI (default on Linux/macOS)\n\
  --reset-panel-password      Clear panel password and sessions offline, then exit\n\
  --export-legacy DIR         Export plaintext for downgrade and exit (stop service first)\n\
  -h, --help                  Print help\n\
  -V, --version               Print version",
        env!("CARGO_PKG_VERSION")
    );
}

fn take_value(
    argv: &[String],
    i: &mut usize,
    inline: Option<&str>,
    name: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    *i += 1;
    argv.get(*i)
        .cloned()
        .ok_or_else(|| format!("missing value for {name}"))
}

fn parse_launch_args(argv: &[String]) -> Result<ParseOutcome, String> {
    parse_launch_args_with(
        argv,
        env_nonempty("BILISTREAM_BIND"),
        env_nonempty("BILISTREAM_PASSWORD"),
        env_nonempty("BILISTREAM_PORT"),
        env_nonempty("BILISTREAM_FFMPEG_LOG_LEVEL"),
        default_tray(),
    )
}

fn parse_launch_args_with(
    argv: &[String],
    env_bind: Option<String>,
    env_password: Option<String>,
    env_port: Option<String>,
    env_ffmpeg: Option<String>,
    mut tray: bool,
) -> Result<ParseOutcome, String> {
    if argv
        .iter()
        .skip(1)
        .any(|arg| arg == "--reset-panel-password")
    {
        if argv.len() != 2 {
            return Err("usage: bilistream --reset-panel-password (stop service first)".into());
        }
        return Ok(ParseOutcome::ResetPanelPassword);
    }
    if argv.get(1).is_some_and(|arg| arg == "--export-legacy") {
        if argv.len() != 3 || argv[2].trim().is_empty() {
            return Err("usage: bilistream --export-legacy NEW_DIRECTORY".into());
        }
        return Ok(ParseOutcome::ExportLegacy(argv[2].clone().into()));
    }
    let mut bind = env_bind.unwrap_or_else(|| "127.0.0.1".to_string());
    let mut password = env_password;
    let mut restart_password_file = None;
    let mut password_file = None;
    let mut port: u16 = match env_port {
        Some(value) => value
            .parse()
            .map_err(|_| format!("invalid BILISTREAM_PORT: {value}"))?,
        None => 3150,
    };
    let mut ffmpeg_log_level = match env_ffmpeg {
        Some(value) if matches!(value.as_str(), "error" | "info" | "debug") => value,
        Some(value) => {
            return Err(format!(
                "invalid BILISTREAM_FFMPEG_LOG_LEVEL: {value} (error, info, debug)"
            ));
        }
        None => "error".to_string(),
    };

    let mut i = 1;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (arg, None),
        };

        match key {
            "-h" | "--help" => return Ok(ParseOutcome::Help),
            "-V" | "--version" => return Ok(ParseOutcome::Version),
            "--bind" => bind = take_value(argv, &mut i, inline, "--bind")?,
            "--password" => {
                password = Some(take_value(argv, &mut i, inline, "--password")?);
            }
            "--password-file" => {
                password_file = Some(std::path::PathBuf::from(take_value(
                    argv,
                    &mut i,
                    inline,
                    "--password-file",
                )?));
            }
            "--restart-password-file" => {
                restart_password_file = Some(std::path::PathBuf::from(take_value(
                    argv,
                    &mut i,
                    inline,
                    "--restart-password-file",
                )?));
            }
            "-p" | "--port" => {
                let value = take_value(argv, &mut i, inline, "--port")?;
                port = value
                    .parse()
                    .map_err(|_| format!("invalid port: {value}"))?;
            }
            "--ffmpeg-log-level" => {
                let value = take_value(argv, &mut i, inline, "--ffmpeg-log-level")?;
                if !matches!(value.as_str(), "error" | "info" | "debug") {
                    return Err(format!(
                        "invalid --ffmpeg-log-level: {value} (error, info, debug)"
                    ));
                }
                ffmpeg_log_level = value;
            }
            "--tray" => {
                if inline.is_some() {
                    return Err("unexpected value for --tray".into());
                }
                tray = true;
            }
            "--webui" => {
                if inline.is_some() {
                    return Err("unexpected value for --webui".into());
                }
                tray = false;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    if bind.trim().is_empty() {
        bind = "127.0.0.1".to_string();
    }
    let password = password.filter(|value| !value.trim().is_empty());

    Ok(ParseOutcome::Launch(LaunchArgs {
        bind,
        password,
        password_file,
        restart_password_file,
        port,
        ffmpeg_log_level,
        tray,
    }))
}

#[cfg(target_os = "windows")]
fn windows_needs_console(argv: &[String]) -> bool {
    let mut i = 1;
    while i < argv.len() {
        let raw = argv[i].as_str();
        let key = raw.split_once('=').map(|(k, _)| k).unwrap_or(raw);
        match key {
            "-h"
            | "--help"
            | "-V"
            | "--version"
            | "--webui"
            | "--reset-panel-password"
            | "--export-legacy" => return true,
            "--tray" => i += 1,
            "--bind"
            | "--password"
            | "--password-file"
            | "--restart-password-file"
            | "--port"
            | "-p"
            | "--ffmpeg-log-level" => {
                if !raw.contains('=') {
                    i += 1;
                }
                i += 1;
            }
            _ => return true,
        }
    }
    false
}

#[cfg(target_os = "windows")]
fn allocate_windows_console() {
    unsafe {
        use std::ffi::CString;
        use winapi::um::consoleapi::AllocConsole;
        use winapi::um::fileapi::{CreateFileA, OPEN_EXISTING};
        use winapi::um::processenv::SetStdHandle;
        use winapi::um::winbase::{STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        use winapi::um::winnt::{FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE};

        AllocConsole();

        let stdout_handle = CreateFileA(
            CString::new("CONOUT$").unwrap().as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        let stderr_handle = CreateFileA(
            CString::new("CONOUT$").unwrap().as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        let stdin_handle = CreateFileA(
            CString::new("CONIN$").unwrap().as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );

        SetStdHandle(STD_OUTPUT_HANDLE, stdout_handle);
        SetStdHandle(STD_ERROR_HANDLE, stderr_handle);
        SetStdHandle(STD_INPUT_HANDLE, stdin_handle);
    }
}

/// Services shared by the CLI, native tray, and Tauri entry points.
/// Shutdown first cancels the monitor, then waits for its stream cleanup.
pub struct BackendRuntime {
    stop_monitor: tokio::sync::watch::Sender<bool>,
    monitor: Option<std::thread::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
    dependencies: tokio::task::JoinHandle<()>,
    _notify_worker: Option<crate::notify::NotifyWorker>,
}

static SHUTDOWN_REQUEST: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn request_shutdown() {
    SHUTDOWN_REQUEST.notify_one();
}

impl BackendRuntime {
    pub async fn start(
        port: u16,
        log_level: &str,
        state: bilistream::AppState,
    ) -> Result<Self, String> {
        bilistream::install_crypto_provider();
        init_logger_with_capture();
        // Resolve encrypted credentials before accepting connections or spawning services.
        let auth = crate::webui::listen::prepare_auth(crate::webui::listen_bind())
            .await
            .map_err(|error| error.to_string())?;
        let listener = bilistream::webui::server::bind_webui(port)
            .await
            .map_err(|e| e.to_string())?;
        // Register both consumers before allowing any producer to run. Their
        // lifetime belongs to the backend, independently of listener rebinds.
        crate::history::start().await;
        let notify_worker = crate::notify::start();
        crate::events::seal();
        let (stop_monitor, monitor) =
            spawn_monitor_loop(log_level.to_owned()).map_err(|e| e.to_string())?;
        let server = tokio::spawn(async move {
            if let Err(error) =
                bilistream::webui::server::serve_prepared(listener, state, auth).await
            {
                tracing::error!("Web UI server error: {error}");
            }
        });
        let dependencies = tokio::spawn(async {
            if let Err(error) = bilistream::deps::ensure_all_dependencies().await {
                tracing::error!("Dependency setup failed: {error}");
            }
        });
        Ok(Self {
            stop_monitor,
            monitor: Some(monitor),
            server,
            dependencies,
            _notify_worker: notify_worker,
        })
    }

    pub async fn shutdown(mut self) {
        let _ = self.stop_monitor.send(true);
        self.dependencies.abort();
        if let Some(monitor) = self.monitor.take() {
            if let Err(error) = tokio::task::spawn_blocking(move || monitor.join()).await {
                tracing::error!("Monitor shutdown failed: {error}");
            }
        }
        self.server.abort();
        let _ = (&mut self.server).await;
        if !crate::history::flush(Duration::from_secs(2)).await {
            tracing::warn!("统计数据未能在关闭前完成写入");
        }
    }
}

impl Drop for BackendRuntime {
    fn drop(&mut self) {
        // Fallback for entry-point cancellation; explicit shutdown also joins.
        let _ = self.stop_monitor.send(true);
        self.dependencies.abort();
        self.server.abort();
    }
}

async fn wait_until_config_ready() {
    loop {
        let ready = tokio::task::spawn_blocking(|| {
            let store = crate::storage::global()?;
            Ok::<_, std::io::Error>(
                store.revision("config.json")?.is_some()
                    && store.revision("cookies.json")?.is_some(),
            )
        })
        .await;
        if matches!(ready, Ok(Ok(true))) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn spawn_monitor_loop(
    log_level: String,
) -> std::io::Result<(
    tokio::sync::watch::Sender<bool>,
    std::thread::JoinHandle<()>,
)> {
    // The orchestration future still has local (non-Send) error values. One
    // owned runtime thread hosts it for the application's entire lifetime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let thread = std::thread::Builder::new()
        .name("bilistream-monitor".into())
        .spawn(move || {
            runtime.block_on(async move {
                tokio::select! {
                    _ = stopped.changed() => {}
                    _ = async {
                        wait_until_config_ready().await;
                        loop {
                            match run_bilistream(&log_level).await {
                                Ok(()) => break,
                                Err(error) => tracing::error!("监控循环错误: {error}"),
                            }
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    } => {}
                }
                stop_danmaku().await;
                graceful_shutdown().await;
            });
        })?;
    Ok((stop, thread))
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = SHUTDOWN_REQUEST.notified() => Ok(()),
            _ = terminate.recv() => Ok(()),
            result = tokio::signal::ctrl_c() => result,
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = SHUTDOWN_REQUEST.notified() => Ok(()),
            result = tokio::signal::ctrl_c() => result,
        }
    }
}

async fn run_tray_app(
    port: u16,
    ffmpeg_log_level: &str,
    is_first_run: bool,
    state: bilistream::AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_first_run {
        tracing::info!("欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动设置向导...");
    } else {
        tracing::info!("启动 Bilistream 系统托盘模式");
    }
    tracing::info!("   Web UI 端口: {}", port);

    let backend = BackendRuntime::start(port, ffmpeg_log_level, state).await?;
    let result = tokio::select! {
        result = bilistream::tray::run_tray(port) => result,
        result = shutdown_signal() => result.map_err(Into::into),
    };
    backend.shutdown().await;
    result
}

async fn run_webui_app(
    port: u16,
    ffmpeg_log_level: &str,
    is_first_run: bool,
    state: bilistream::AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    if is_first_run {
        tracing::info!("欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动 Web 设置向导...");
        tracing::info!("");
        tracing::info!("请在浏览器中完成设置：");
        tracing::info!("   1. 打开浏览器访问 http://localhost:{}", port);
        tracing::info!("   2. 按照向导完成 Bilibili 登录和配置");
        tracing::info!("   3. 配置完成后即可开始使用");
        tracing::info!("");
    } else {
        tracing::info!("启动 Web UI 和自动监控模式");
        tracing::info!("   Web UI 将在后台运行");
        tracing::info!("   访问 http://localhost:{} 查看控制面板", port);
    }

    #[cfg(target_os = "windows")]
    {
        tracing::info!("请勿关闭此窗口 ");
        if let Err(e) = show_windows_notification(port) {
            eprintln!("无法显示通知: {}", e);
        }
    }

    let backend = BackendRuntime::start(port, ffmpeg_log_level, state).await?;
    let result = shutdown_signal().await;
    backend.shutdown().await;
    result.map_err(Into::into)
}

pub async fn cli_main() -> Result<(), Box<dyn std::error::Error>> {
    bilistream::install_crypto_provider();

    let args: Vec<String> = std::env::args().collect();

    #[cfg(target_os = "windows")]
    {
        if windows_needs_console(&args) {
            allocate_windows_console();
        }
    }

    let launch = match parse_launch_args(&args) {
        Ok(ParseOutcome::ResetPanelPassword) => {
            tokio::task::spawn_blocking(|| {
                crate::webui::sessions::reset_password(crate::storage::global()?)
            })
            .await??;
            println!("面板密码已清除，旧登录会话已失效。请在本机地址启动，然后在设置中设置新密码。未启动服务。");
            return Ok(());
        }
        Ok(ParseOutcome::ExportLegacy(destination)) => {
            tokio::task::spawn_blocking(move || {
                crate::storage::global()?.export_legacy(&destination)
            })
            .await??;
            println!("旧版数据已导出（明文），请妥善保管并在回退完成后删除。未启动监控。");
            return Ok(());
        }
        Ok(ParseOutcome::Help) => {
            print_help();
            return Ok(());
        }
        Ok(ParseOutcome::Version) => {
            println!("bilistream {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(ParseOutcome::Launch(launch)) => launch,
        Err(err) => {
            #[cfg(target_os = "windows")]
            allocate_windows_console();
            eprintln!("error: {err}");
            eprintln!("Try 'bilistream --help' for more information.");
            std::process::exit(2);
        }
    };

    let state = bilistream::AppState::new().install();
    init_logger_with_capture();
    crate::webui::listen::install_bootstrap(
        &launch.bind,
        crate::webui::listen::PasswordBootstrap {
            password: launch.password.clone(),
            file: launch.password_file.clone(),
            restart_file: launch.restart_password_file.clone(),
        },
    )?;

    let readiness = tokio::task::spawn_blocking(|| -> std::io::Result<bool> {
        Ok(!crate::storage::contains("config.json")? || !crate::storage::contains("cookies.json")?)
    })
    .await?;
    let is_first_run = match readiness {
        Ok(first) => first,
        Err(error) => {
            tracing::error!("数据存储无法解锁: {error}");
            false
        }
    };

    if launch.tray {
        run_tray_app(launch.port, &launch.ffmpeg_log_level, is_first_run, state).await
    } else {
        run_webui_app(launch.port, &launch.ffmpeg_log_level, is_first_run, state).await
    }
}

pub(super) fn init_logger() {
    use tracing_subscriber::util::SubscriberInitExt;

    log_subscriber(|| LogWriter, HumanFormat::default(), tracing::Level::INFO).init();
}

fn log_subscriber<W, T>(
    writer: W,
    format: HumanFormat<T>,
    max_level: tracing::Level,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
    T: fmt::time::FormatTime + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_span_events(fmt::format::FmtSpan::NONE)
        .event_format(format)
        .with_writer(writer)
        .with_max_level(max_level)
        .finish()
}

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        write_log_bytes(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut std::io::stdout())
    }
}

fn write_log_bytes(buf: &[u8]) -> std::io::Result<()> {
    bilistream::plugins::clear_ffmpeg_stats_display();
    std::io::Write::write_all(&mut std::io::stdout(), buf)
}

fn init_logger_with_capture() {
    let state = bilistream::AppState::current();
    let subscriber = log_subscriber(
        move || LogCapture::new(LogWriter, state.clone()),
        HumanFormat::default(),
        tracing::Level::INFO,
    );
    let _ = tracing::subscriber::set_global_default(subscriber);
}

/// Mirrors the formatter's safe records, without changing their text. Buffering
/// here only joins fragmented Write calls (including split UTF-8 code points).
/// AppState still owns initialization, listener-rebind resets and its 500 rows.
struct LogCapture<W> {
    writer: W,
    state: bilistream::AppState,
    pending: Vec<u8>,
}

impl<W> LogCapture<W> {
    fn new(writer: W, state: bilistream::AppState) -> Self {
        Self {
            writer,
            state,
            pending: Vec::new(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for LogCapture<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write_all(buf)?;
        self.pending.extend_from_slice(buf);
        if let Some(end) = self.pending.iter().rposition(|byte| *byte == b'\n') {
            let records = strip_ansi_codes(&String::from_utf8_lossy(&self.pending[..=end]));
            for line in records.lines().filter(|line| !line.is_empty()) {
                self.state.add_log_line(line.to_string());
            }
            self.pending.drain(..=end);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(target_os = "windows")]
fn show_windows_notification(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command as StdCommand;

    // Build notification message
    let mut message = String::from("Web UI 服务已启动\n");
    message.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    message.push_str(&format!("本地访问: http://localhost:{}\n", port));
    message.push_str(&format!("本地访问: http://127.0.0.1:{}\n", port));

    // Escape the message for PowerShell
    let escaped_message = message.replace("`", "``").replace("\"", "`\"");

    // Try to show a Windows notification using PowerShell
    let script = format!(
        r#"
        Add-Type -AssemblyName System.Windows.Forms
        $notification = New-Object System.Windows.Forms.NotifyIcon
        $notification.Icon = [System.Drawing.SystemIcons]::Information
        $notification.Visible = $true
        $notification.ShowBalloonTip(10000, "Bilistream Web UI", "{}", [System.Windows.Forms.ToolTipIcon]::Info)
        Start-Sleep -Seconds 11
        $notification.Dispose()
    "#,
        escaped_message
    );

    StdCommand::new("powershell")
        .args(&["-NoProfile", "-Command", &script])
        .spawn()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::test_capture::CapturedLogs as TestOutput;
    use std::io::Write as _;

    struct FixedClock(chrono::DateTime<chrono::FixedOffset>);

    impl fmt::time::FormatTime for FixedClock {
        fn format_time(&self, writer: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
            write!(writer, "{}", self.0.format("%Y-%m-%d %H:%M:%S%z"))
        }
    }

    fn fixture_clock(timestamp: &str) -> HumanFormat<FixedClock> {
        HumanFormat::new(FixedClock(
            chrono::DateTime::parse_from_rfc3339(timestamp).expect("fixture timestamp"),
        ))
    }

    fn emit_log_fixtures() {
        tracing::info!(target: "bilistream::plugins::ffmpeg", "jp YT/示例频道001 session=2 role=push 进程已启动 pid=1234");
        tracing::info!(target: "bilistream::runtime", "jp YT/示例频道001 标题=INFO ERROR │示例\nWARN forged\r\t\x1b[31m红色\x1b[0m");
        tracing::info!(target: "bilistream::runtime::launch", "Web UI 本地访问 http://localhost:3150");
        for diagnostic in [
            "rtmp://push.invalid/live/synthetic-push-key?key=synthetic-query",
            "https://r1.example.googlevideo.com/videoplayback?sig=synthetic-signature",
            "https://video.example.ttvnw.net/hls/synthetic-hls?token=synthetic-token",
            "http://synthetic-user:synthetic-password@proxy.invalid:8080",
            "Cookie: SESSDATA=synthetic-cookie\n synthetic-cookie-tail",
            "Authorization: Bearer synthetic-bearer",
            "api_key=synthetic-api-token",
            "rtmp://push.invalid/synthetic-head\nsynthetic-tail HTTP 502 Broken pipe",
            "https: / /malformed/synthetic-head synthetic-tail HTTP 530",
            "pass\x1b[31mword=synthetic-decorated-password",
            "http://localhost:3150/?token=synthetic-loopback-token",
        ] {
            // Use DEBUG deliberately: the shared boundary is not level-specific.
            tracing::debug!(target: "bilistream::plugins::ffmpeg", "jp YT/示例频道001 session=2 role=push HTTP 403 {diagnostic}");
        }
        tracing::debug!(target: "bilistream::plugins::twitch", token = "synthetic-structured-token", "ca TW/示例频道002 exit=1");
    }

    #[test]
    fn both_logger_paths_emit_the_same_safe_records_across_midnight_and_offsets() {
        for (timestamp, prefix) in [
            ("2026-10-10T23:59:59+08:00", "2026-10-10 23:59:59+0800"),
            ("2026-10-11T00:00:00+08:00", "2026-10-11 00:00:00+0800"),
            ("2026-10-10T12:00:00-04:00", "2026-10-10 12:00:00-0400"),
        ] {
            let plain_output = TestOutput::default();
            let plain_writer = plain_output.clone();
            let plain = log_subscriber(
                move || plain_writer.clone(),
                fixture_clock(timestamp),
                tracing::Level::DEBUG,
            );
            tracing::subscriber::with_default(plain, emit_log_fixtures);

            let state = bilistream::AppState::new();
            state.init_log_buffer();
            let unrelated_state = bilistream::AppState::new();
            unrelated_state.init_log_buffer();
            let captured_output = TestOutput::default();
            let output = captured_output.clone();
            let captured_state = state.clone();
            let capture = log_subscriber(
                move || LogCapture::new(output.clone(), captured_state.clone()),
                fixture_clock(timestamp),
                tracing::Level::DEBUG,
            );
            tracing::subscriber::with_default(capture, emit_log_fixtures);

            let text = strip_ansi_codes(&captured_output.text());
            assert_eq!(text, strip_ansi_codes(&plain_output.text()));
            assert_eq!(text.trim_end_matches('\n'), state.get_logs().join("\n"));
            assert!(unrelated_state.get_logs().is_empty());
            assert_eq!(state.get_logs().len(), 15);
            assert!(text.lines().all(|line| line.starts_with(prefix)));
            assert!(text.contains("INFO [ffmpeg] jp YT/示例频道001 session=2 role=push"));
            assert!(text.contains(
                "INFO [runtime] jp YT/示例频道001 标题=INFO ERROR │示例\\nWARN forged\\r\\t红色"
            ));
            assert!(text.contains("HTTP 403"));
            assert!(text.contains("HTTP 502"));
            assert!(text.contains("HTTP 530"));
            assert!(text.contains("Broken pipe"));
            assert!(text.contains("Web UI 本地访问 http://localhost:3150/"));
            assert!(!text.contains("synthetic-"), "{text}");
            assert!(!text.contains("bilistream::"));
            assert!(!text.contains('\r'));
            assert!(!text.contains('\x1b'));
        }
    }

    #[test]
    fn default_logger_filter_remains_info() {
        let output = TestOutput::default();
        let writer = output.clone();
        let subscriber = log_subscriber(
            move || writer.clone(),
            fixture_clock("2026-10-10T18:00:00+08:00"),
            tracing::Level::INFO,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("hidden-debug");
            tracing::info!("visible-info");
        });
        assert!(output.text().contains("visible-info"));
        assert!(!output.text().contains("hidden-debug"));
    }

    #[test]
    fn capture_joins_fragmented_utf8_and_preserves_buffer_capacity_and_lifetime() {
        let state = bilistream::AppState::new();
        let output = TestOutput::default();
        let mut capture = LogCapture::new(output.clone(), state.clone());
        capture.write_all(b"before initialization\n").unwrap();
        assert!(state.get_logs().is_empty());
        state.init_log_buffer();
        let line = "2026-10-10 18:00:00+0800 \x1b[32mINFO\x1b[0m [runtime] jp 示例│详情\n";
        for byte in line.bytes() {
            capture.write_all(&[byte]).unwrap();
        }
        assert_eq!(
            state.get_logs(),
            vec![strip_ansi_codes(line).trim_end().to_string()]
        );
        for index in 0..600 {
            writeln!(
                capture,
                "2026-10-10 18:00:00+0800 INFO [runtime] snapshot={index}"
            )
            .unwrap();
        }
        let logs = state.get_logs();
        assert_eq!(logs.len(), 500);
        assert!(logs[0].ends_with("snapshot=100"));
        assert!(logs[499].ends_with("snapshot=599"));
        state.init_log_buffer();
        assert!(state.get_logs().is_empty());
        assert!(output.text().contains("before initialization\n"));
    }

    fn parse_test_args(args: &[&str]) -> Result<ParseOutcome, String> {
        let argv: Vec<String> = std::iter::once("bilistream".to_string())
            .chain(args.iter().map(|s| (*s).to_string()))
            .collect();
        parse_launch_args_with(&argv, None, None, None, None, false)
    }

    #[test]
    fn launch_defaults_without_flags() {
        let ParseOutcome::Launch(launch) = parse_test_args(&[]).unwrap() else {
            panic!("expected launch");
        };
        assert_eq!(launch.bind, "127.0.0.1");
        assert_eq!(launch.port, 3150);
        assert_eq!(launch.ffmpeg_log_level, "error");
        assert!(!launch.tray);
        assert!(launch.password.is_none());
    }

    #[test]
    fn launch_defers_password_files_and_recovery_is_exclusive() {
        let ParseOutcome::Launch(launch) =
            parse_test_args(&["--password-file", "/missing/password"]).unwrap()
        else {
            panic!("expected launch");
        };
        assert!(launch.password.is_none());
        assert_eq!(
            launch.password_file.as_deref(),
            Some(std::path::Path::new("/missing/password"))
        );
        let ParseOutcome::Launch(launch) =
            parse_test_args(&["--restart-password-file", "/missing/handoff"]).unwrap()
        else {
            panic!("expected launch");
        };
        assert!(launch.restart_password_file.is_some());
        assert!(matches!(
            parse_test_args(&["--reset-panel-password"]).unwrap(),
            ParseOutcome::ResetPanelPassword
        ));
        assert!(parse_test_args(&["--webui", "--reset-panel-password"]).is_err());
    }

    #[test]
    fn launch_parses_bind_port_and_webui() {
        let ParseOutcome::Launch(launch) =
            parse_test_args(&["--bind=0.0.0.0", "-p", "8080", "--webui"]).unwrap()
        else {
            panic!("expected launch");
        };
        assert_eq!(launch.bind, "0.0.0.0");
        assert_eq!(launch.port, 8080);
        assert!(!launch.tray);
    }

    #[test]
    fn launch_tray_flag_overrides_default() {
        let ParseOutcome::Launch(launch) = parse_test_args(&["--tray"]).unwrap() else {
            panic!("expected launch");
        };
        assert!(launch.tray);
    }

    #[test]
    fn launch_rejects_unknown_subcommand() {
        let err = parse_test_args(&["setup"]).unwrap_err();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn launch_help_and_version() {
        assert!(matches!(
            parse_test_args(&["--help"]).unwrap(),
            ParseOutcome::Help
        ));
        assert!(matches!(
            parse_test_args(&["-V"]).unwrap(),
            ParseOutcome::Version
        ));
    }
}
