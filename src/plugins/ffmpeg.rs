use std::fs;
use std::path::Path;
use std::process::Command;

/// Checks if the ffmpeg lock file exists.
pub fn is_ffmpeg_running() -> bool {
    Path::new("ffmpeg.lock").exists()
}

/// Creates the ffmpeg lock file.
pub fn create_ffmpeg_lock() -> std::io::Result<()> {
    fs::File::create("ffmpeg.lock")?;
    println!("ffmpeg lock file created");
    Ok(())
}

/// Removes the ffmpeg lock file.
pub fn remove_ffmpeg_lock() -> std::io::Result<()> {
    fs::remove_file("ffmpeg.lock")?;
    println!("ffmpeg lock file removed");
    Ok(())
}

/// Executes the ffmpeg command with the provided parameters.
/// Prevents multiple instances from running simultaneously using a lock file.
pub fn ffmpeg(rtmp_url: String, rtmp_key: String, m3u8_url: String, ffmpeg_proxy: Option<String>) {
    // Check if ffmpeg is already running
    if is_ffmpeg_running() {
        println!("ffmpeg is already running. Skipping new instance.");
        return;
    }

    // Create the lock file
    if let Err(e) = create_ffmpeg_lock() {
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
        .arg("error")
        .arg("-stats")
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
    if let Err(e) = remove_ffmpeg_lock() {
        println!("Failed to remove ffmpeg lock file: {}", e);
    }
}
