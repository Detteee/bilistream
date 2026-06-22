use std::error::Error;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
const DETACHED_PROCESS: u32 = 0x0000_0008;

pub fn executable_command(windows_name: &str, default_name: &str) -> String {
    if cfg!(target_os = "windows") {
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let local_exe = exe_dir.join(windows_name);
                if local_exe.exists() {
                    return local_exe.to_string_lossy().to_string();
                }
            }
        }
        windows_name.to_string()
    } else {
        default_name.to_string()
    }
}

#[cfg(target_os = "windows")]
pub fn configure_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(not(target_os = "windows"))]
pub fn configure_no_window(_cmd: &mut Command) {}

#[cfg(target_os = "windows")]
pub fn configure_tokio_no_window(cmd: &mut tokio::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(not(target_os = "windows"))]
pub fn configure_tokio_no_window(_cmd: &mut tokio::process::Command) {}

pub fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, Box<dyn Error>> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} timed out after {} seconds", label, timeout.as_secs()).into());
        }

        thread::sleep(Duration::from_millis(200));
    }
}

pub fn add_yt_dlp_cookies_args(
    command: &mut Command,
    cookies_file: &Option<String>,
    cookies_from_browser: &Option<String>,
) {
    if let Some(browser) = cookies_from_browser {
        if !browser.is_empty() {
            command.arg("--cookies-from-browser");
            command.arg(browser);
            return;
        }
    }

    if let Some(file_path) = cookies_file {
        if !file_path.is_empty() {
            command.arg("--cookies");
            command.arg(file_path);
        }
    }
}

pub fn set_high_priority(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("renice")
            .arg("-n")
            .arg("-10")
            .arg("-p")
            .arg(pid.to_string())
            .output();

        match status {
            Ok(output) if output.status.success() => {}
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
        let status = Command::new("wmic")
            .arg("process")
            .arg("where")
            .arg(format!("ProcessId={}", pid))
            .arg("CALL")
            .arg("setpriority")
            .arg("128")
            .output();

        match status {
            Ok(output) if output.status.success() => {}
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
        let status = Command::new("renice")
            .arg("-n")
            .arg("-10")
            .arg("-p")
            .arg(pid.to_string())
            .output();

        match status {
            Ok(output) if output.status.success() => {}
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
