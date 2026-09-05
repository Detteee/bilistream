use md5::{Digest, Md5};
use std::fmt::Write as _;
use std::io;
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Semaphore;

const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
static PROCESS_SLOTS: Semaphore = Semaphore::const_new(4);

/// Lowercase hex MD5, the digest form Bilibili's signed endpoints expect.
pub fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let mut digest = String::with_capacity(32);
    for byte in hasher.finalize() {
        let _ = write!(digest, "{:02x}", byte);
    }
    digest
}

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
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(not(target_os = "windows"))]
pub fn configure_tokio_no_window(_cmd: &mut tokio::process::Command) {}

async fn read_limited(reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "subprocess output limit exceeded",
        ));
    }
    Ok(bytes)
}

/// Drain both pipes concurrently with exit, including queueing in the deadline.
/// Dropping this future also terminates the direct child.
pub async fn command_output_with_timeout(
    command: Command,
    limit: Duration,
    label: &str,
) -> io::Result<Output> {
    let deadline = tokio::time::Instant::now() + limit;
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{label} timed out after {limit:?}"),
        )
    };
    let _permit = tokio::time::timeout_at(deadline, PROCESS_SLOTS.acquire())
        .await
        .map_err(|_| timed_out())?
        .map_err(io::Error::other)?;
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("stderr pipe missing"))?;
    let result = tokio::time::timeout_at(deadline, async {
        let (status, stdout, stderr) =
            tokio::try_join!(child.wait(), read_limited(stdout), read_limited(stderr))?;
        Ok::<_, io::Error>(Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    let error = match result {
        Ok(Ok(output)) => return Ok(output),
        Ok(Err(error)) => error,
        Err(_) => timed_out(),
    };
    // Reap on normal failure; kill_on_drop covers caller cancellation.
    let _ = child.kill().await;
    let _ = child.wait().await;
    Err(error)
}

/// Build a direct CDN thumbnail URL for YT/TW streams (same logic as holodex webui).
pub fn stream_thumbnail_url(
    platform: &str,
    channel_id: &str,
    stream_id: Option<&str>,
    holodex_thumbnail: Option<&str>,
) -> Option<String> {
    if let Some(url) = holodex_thumbnail.filter(|url| !url.is_empty()) {
        return Some(url.to_string());
    }

    match platform {
        "YT" => stream_id
            .filter(|id| !id.is_empty())
            .map(|id| format!("https://i.ytimg.com/vi/{id}/sddefault.jpg")),
        "TW" if !channel_id.is_empty() => Some(format!(
            "https://static-cdn.jtvnw.net/previews-ttv/live_user_{channel_id}-640x360.jpg"
        )),
        _ => None,
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

#[cfg(test)]
mod process_tests {
    use super::*;

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "plugins::utils::process_tests::subprocess_fixture",
            "--ignored",
            "--nocapture",
        ]);
        command.env("BILISTREAM_PROCESS_TEST", mode);
        command
    }

    // Run in a child test process, so tests do not depend on Python or a shell.
    #[test]
    #[ignore = "subprocess fixture"]
    fn subprocess_fixture() {
        use std::io::Write;
        match std::env::var("BILISTREAM_PROCESS_TEST").as_deref() {
            Ok("verbose") => {
                std::io::stdout()
                    .write_all(&vec![b'x'; 1024 * 1024])
                    .unwrap();
                std::io::stderr()
                    .write_all(&vec![b'y'; 1024 * 1024])
                    .unwrap();
            }
            Ok("oversize") => {
                let block = [b'x'; 8192];
                for _ in 0..=MAX_OUTPUT_BYTES / block.len() as u64 {
                    if std::io::stdout().write_all(&block).is_err() {
                        break;
                    }
                }
            }
            Ok("sleep") => std::thread::sleep(Duration::from_secs(30)),
            _ => {}
        }
    }

    #[tokio::test]
    async fn drains_both_pipes_larger_than_os_buffers() {
        let output =
            command_output_with_timeout(fixture("verbose"), Duration::from_secs(10), "fixture")
                .await
                .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.iter().filter(|&&b| b == b'x').count() >= 1024 * 1024);
        assert_eq!(output.stderr, vec![b'y'; 1024 * 1024]);
    }

    #[tokio::test]
    async fn rejects_excessive_output() {
        let error =
            command_output_with_timeout(fixture("oversize"), Duration::from_secs(10), "fixture")
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn enforces_deadline_without_blocking_runtime() {
        let started = tokio::time::Instant::now();
        let error =
            command_output_with_timeout(fixture("sleep"), Duration::from_millis(100), "fixture")
                .await
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[cfg(test)]
mod md5_hex_tests {
    use super::md5_hex;

    #[test]
    fn matches_known_digests() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex("k"), "8ce4b16b22b58894aa86c421e8759df3");
        // Leading 0x0e byte: guards against a non-padded hex formatter.
        assert_eq!(md5_hex("240610708"), "0e462097431906509019562988736854");
    }
}
