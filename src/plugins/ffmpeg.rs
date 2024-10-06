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
    println!("{} created", lock_file);
    Ok(())
}

/// Removes the ffmpeg lock file for the specified platform.
pub fn remove_ffmpeg_lock(platform: &str) -> std::io::Result<()> {
    let lock_file = format!("ffmpeg.lock-{}", platform);
    fs::remove_file(&lock_file)?;
    println!("{} removed", lock_file);
    Ok(())
}

/// Executes the ffmpeg command with the provided parameters.
/// Prevents multiple instances from running simultaneously using platform-specific lock files.
pub fn ffmpeg(
    rtmp_url: String,
    rtmp_key: String,
    m3u8_url: String,
    ffmpeg_proxy: Option<String>,
    log_level: &str,
    platform: &str,
) {
    // Check if any ffmpeg is already running
    if is_any_ffmpeg_running() {
        println!("An ffmpeg instance is already running. Skipping new instance.");
        return;
    }

    // Create the lock file for the specified platform
    if let Err(e) = create_ffmpeg_lock(platform) {
        println!("Failed to create ffmpeg lock file: {}", e);
        return;
    }

    let cmd = format!("{}{}", rtmp_url, rtmp_key);
    let mut command = Command::new("ffmpeg");

    if let Some(proxy) = ffmpeg_proxy {
        command.arg("-http_proxy").arg(proxy);
    }

    command
        .arg("-loglevel")
        .arg(log_level)
        .arg("-stats")
        .arg("-re")
        .arg("-i")
        .arg(m3u8_url)
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("flv")
        .arg(cmd);

    match command.status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                println!("ffmpeg exited with status code: {}", code);
            } else {
                println!("ffmpeg terminated by signal");
            }
        }
        Err(e) => println!("Failed to execute ffmpeg: {}", e),
    }

    // Remove the lock file after ffmpeg finishes
    if let Err(e) = remove_ffmpeg_lock(platform) {
        println!("Failed to remove ffmpeg lock file: {}", e);
    }
}
