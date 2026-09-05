//! CLI options, shared application lifecycle, and log capture.

use super::{graceful_shutdown, run_bilistream};
use crate as bilistream;
use crate::plugins::stop_danmaku;
use std::time::Duration;
use tracing_subscriber::fmt;

#[derive(Debug)]
struct LaunchArgs {
    bind: String,
    password: Option<String>,
    port: u16,
    ffmpeg_log_level: String,
    tray: bool,
}

#[derive(Debug)]
enum ParseOutcome {
    Launch(LaunchArgs),
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
  --password PASSWORD         Web UI login password (or BILISTREAM_PASSWORD)\n\
  --ffmpeg-log-level LEVEL    error, info, or debug (default error)\n\
  --tray                      System tray (default on Windows)\n\
  --webui                     Console Web UI (default on Linux/macOS)\n\
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
    let mut bind = env_bind.unwrap_or_else(|| "127.0.0.1".to_string());
    let mut password = env_password;
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
            "-h" | "--help" | "-V" | "--version" | "--webui" => return true,
            "--tray" => i += 1,
            "--bind" | "--password" | "--port" | "-p" | "--ffmpeg-log-level" => {
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

fn apply_webui_listen(
    bind: &str,
    password: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bind = if bind.trim().is_empty() {
        "127.0.0.1"
    } else {
        bind
    };
    let password = password.filter(|value| !value.trim().is_empty());
    bilistream::webui::install_listen(bind, password)?;
    Ok(())
}

/// Services shared by the CLI, native tray, and Tauri entry points.
/// Shutdown first cancels the monitor, then waits for its stream cleanup.
pub struct BackendRuntime {
    stop_monitor: tokio::sync::watch::Sender<bool>,
    monitor: Option<std::thread::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
    dependencies: tokio::task::JoinHandle<()>,
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
        // Binding is the readiness signal; no arbitrary sleep or credential
        // operation stands between startup and the recovery interface.
        let listener = bilistream::webui::server::bind_webui(port)
            .await
            .map_err(|e| e.to_string())?;
        let (stop_monitor, monitor) =
            spawn_monitor_loop(log_level.to_owned()).map_err(|e| e.to_string())?;
        let server = tokio::spawn(async move {
            if let Err(error) =
                bilistream::webui::server::start_webui_on_listener(listener, state).await
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

fn config_paths() -> Result<(std::path::PathBuf, std::path::PathBuf), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    Ok((
        exe.with_file_name("config.json"),
        exe.with_file_name("cookies.json"),
    ))
}

async fn wait_until_config_ready() {
    let Ok((config_path, cookies_path)) = config_paths() else {
        return;
    };

    if !config_path.exists() {
        tracing::warn!("⚠️ 配置文件不存在，等待用户配置...");
        tracing::info!("💡 请访问 Web UI 进行配置");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if config_path.exists() {
                tracing::info!("✅ 检测到配置文件，开始监控");
                break;
            }
        }
    }

    if !cookies_path.exists() {
        tracing::warn!("⚠️ 登录凭证不存在，等待用户登录...");
        tracing::info!("💡 请访问 Web UI 进行登录");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            if cookies_path.exists() {
                tracing::info!("✅ 检测到登录凭证，开始监控");
                break;
            }
        }
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
        tracing::info!("🚀 欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动设置向导...");
    } else {
        tracing::info!("🚀 启动 Bilistream 系统托盘模式");
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
        tracing::info!("🚀 欢迎使用 Bilistream！");
        tracing::info!("   检测到首次运行，启动 Web 设置向导...");
        tracing::info!("");
        tracing::info!("📋 请在浏览器中完成设置：");
        tracing::info!("   1. 打开浏览器访问 http://localhost:{}", port);
        tracing::info!("   2. 按照向导完成 Bilibili 登录和配置");
        tracing::info!("   3. 配置完成后即可开始使用");
        tracing::info!("");
    } else {
        tracing::info!("🚀 启动 Web UI 和自动监控模式");
        tracing::info!("   Web UI 将在后台运行");
        tracing::info!("   访问 http://localhost:{} 查看控制面板", port);
    }

    #[cfg(target_os = "windows")]
    {
        tracing::info!("⚠️ 请勿关闭此窗口 ⚠️");
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
    apply_webui_listen(&launch.bind, launch.password.clone())?;

    let (config_path, cookies_path) = config_paths()?;
    let is_first_run = !config_path.exists() || !cookies_path.exists();

    if launch.tray {
        run_tray_app(launch.port, &launch.ffmpeg_log_level, is_first_run, state).await
    } else {
        run_webui_app(launch.port, &launch.ffmpeg_log_level, is_first_run, state).await
    }
}

pub(super) fn init_logger() {
    tracing_subscriber::fmt()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_writer(|| LogWriter)
        .with_max_level(tracing::Level::INFO)
        .init();
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
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::SubscriberExt;

    // Create a custom writer that captures logs
    struct LogCapture;

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(s) = std::str::from_utf8(buf) {
                write_log_bytes(buf)?;
                // Capture for web UI (strip ANSI codes)
                // First strip ANSI codes from the entire string
                let clean_str = strip_ansi_codes(s);
                // Then split into lines
                for line in clean_str.lines() {
                    // Skip pure box drawing lines (borders only)
                    let trimmed = line.trim();
                    if trimmed.starts_with('┌')
                        || trimmed.starts_with('├')
                        || trimmed.starts_with('└')
                    {
                        continue;
                    }

                    // For lines with content, strip the box borders but keep the content
                    let content = if line.contains('│') {
                        // Extract content between │ characters
                        line.split('│')
                            .filter(|s| !s.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .trim()
                            .to_string()
                    } else {
                        line.to_string()
                    };

                    // Only add non-empty content
                    if !content.is_empty() {
                        bilistream::add_log_line(content);
                    }
                }
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            std::io::stdout().flush()
        }
    }

    // Helper function to strip ANSI escape codes
    fn strip_ansi_codes(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // Skip escape sequence
                if chars.next() == Some('[') {
                    // Skip until we find a letter (end of escape sequence)
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else if ch == '\r' {
                // Skip carriage return
                continue;
            } else {
                result.push(ch);
            }
        }

        result
    }

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_timer(fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_target(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_writer(|| LogCapture);

    let subscriber = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(LevelFilter::INFO);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[cfg(target_os = "windows")]
fn show_windows_notification(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command as StdCommand;

    // Build notification message
    let mut message = String::from("🌐 Web UI 服务已启动\n");
    message.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
    message.push_str(&format!("📍 本地访问: http://localhost:{}\n", port));
    message.push_str(&format!("📍 本地访问: http://127.0.0.1:{}\n", port));

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
