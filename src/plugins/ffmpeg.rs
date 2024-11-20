use std::fs;
use std::path::Path;
use std::process::Command;

/// Checks if any ffmpeg lock file exists.
pub fn is_any_ffmpeg_running() -> bool {
    Path::new("ffmpeg.lock-YT").exists() || Path::new("ffmpeg.lock-TW").exists()
}

/// Checks if the ffmpeg lock file for the specified platform exists.
// pub fn is_ffmpeg_running(platform: &str) -> bool {
//     let lock_file = format!("ffmpeg.lock-{}", platform);
//     Path::new(&lock_file).exists()
// }

/// Creates the ffmpeg lock file for the specified platform.
pub fn create_ffmpeg_lock(platform: &str) -> std::io::Result<()> {
    let lock_file = format!("ffmpeg.lock-{}", platform);
    fs::File::create(&lock_file)?;
    println!("{} 创建成功", lock_file);
    Ok(())
}

/// Removes the ffmpeg lock file for the specified platform.
pub fn remove_ffmpeg_lock(platform: &str) -> std::io::Result<()> {
    let lock_file = format!("ffmpeg.lock-{}", platform);
    fs::remove_file(&lock_file)?;
    println!("{} 删除成功", lock_file);
    Ok(())
}

/// Executes the ffmpeg command with the provided parameters.
/// Prevents multiple instances from running simultaneously using platform-specific lock files.
pub fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    m3u8_url: String,
    proxy: Option<String>,
    log_level: &str,
    platform: &str,
) {
    // Check if any ffmpeg is already running
    if is_any_ffmpeg_running() {
        println!("一个ffmpeg实例已经在运行。跳过新实例。");
        return;
    }

    // Create the lock file for the specified platform
    if let Err(e) = create_ffmpeg_lock(platform) {
        println!("创建ffmpeg锁文件失败: {}", e);
        return;
    }

    let cmd = format!("{}{}", rtmp_url, rtmp_key);
    let mut command = Command::new("ffmpeg");

    if let Some(proxy) = proxy {
        command.arg("-http_proxy").arg(proxy);
    }

    command
        .arg("-i")
        .arg(m3u8_url)
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("flv")
        .arg(cmd)
        .arg("-loglevel")
        .arg(log_level)
        .arg("-stats");

    match command.status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                println!("ffmpeg退出状态码: {}", code);
            } else {
                println!("ffmpeg被信号终止");
            }
        }
        Err(e) => println!("执行ffmpeg失败: {}", e),
    }

    // Remove the lock file after ffmpeg finishes
    if let Err(e) = remove_ffmpeg_lock(platform) {
        println!("删除ffmpeg锁文件失败: {}", e);
    }
}
