use std::error::Error;
#[cfg(target_os = "windows")]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::LockResult;

#[cfg(target_os = "windows")]
use std::io::Write;

// Global download progress tracking
lazy_static::lazy_static! {
    static ref DOWNLOAD_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
    static ref DOWNLOAD_COMPLETE: AtomicBool = AtomicBool::new(false);
    static ref DOWNLOAD_PROGRESS: AtomicUsize = AtomicUsize::new(0);
    static ref DOWNLOAD_TOTAL: AtomicUsize = AtomicUsize::new(0);
    static ref DOWNLOAD_MESSAGE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
}

pub fn is_download_in_progress() -> bool {
    DOWNLOAD_IN_PROGRESS.load(Ordering::Relaxed)
}

pub fn is_download_complete() -> bool {
    DOWNLOAD_COMPLETE.load(Ordering::Relaxed)
}

pub fn get_download_progress() -> (usize, usize, String) {
    let progress = DOWNLOAD_PROGRESS.load(Ordering::Relaxed);
    let total = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
    let message = recover_lock(DOWNLOAD_MESSAGE.lock(), "dependency download message").clone();
    (progress, total, message)
}

fn set_download_message(msg: &str) {
    *recover_lock(DOWNLOAD_MESSAGE.lock(), "dependency download message") = msg.to_string();
    tracing::info!("{}", msg);
}

#[cfg(target_os = "windows")]
const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
#[cfg(target_os = "windows")]
const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";

/// Ensure all required files and dependencies are present
pub async fn ensure_all_dependencies() -> Result<(), Box<dyn Error>> {
    DOWNLOAD_IN_PROGRESS.store(true, Ordering::Relaxed);
    DOWNLOAD_COMPLETE.store(false, Ordering::Relaxed);

    // Count total items to download
    let mut total_items = 0;

    // Check what needs to be downloaded
    let exe_dir = current_exe_dir()?;

    if !crate::storage::contains("areas.json").unwrap_or(false) {
        total_items += 1;
    }
    if !crate::storage::contains("channels.json").unwrap_or(false) {
        total_items += 1;
    }
    total_items += crate::webui::assets::missing_asset_count(&exe_dir);

    #[cfg(target_os = "windows")]
    {
        if !exe_dir.join("yt-dlp.exe").exists() {
            total_items += 1;
        }
        if !exe_dir.join("ffmpeg.exe").exists() {
            total_items += 1;
        }
    }

    DOWNLOAD_TOTAL.store(total_items, Ordering::Relaxed);
    DOWNLOAD_PROGRESS.store(0, Ordering::Relaxed);

    if total_items == 0 {
        set_download_message("所有依赖已就绪");
        DOWNLOAD_COMPLETE.store(true, Ordering::Relaxed);
        DOWNLOAD_IN_PROGRESS.store(false, Ordering::Relaxed);
        return Ok(());
    }

    set_download_message(&format!("准备安装 {} 个文件...", total_items));

    // First, ensure required data files (cross-platform)
    ensure_required_files().await?;

    // Then, ensure platform-specific dependencies
    #[cfg(target_os = "windows")]
    ensure_windows_dependencies().await?;

    #[cfg(not(target_os = "windows"))]
    ensure_linux_dependencies().await?;

    set_download_message("所有依赖已就绪！");
    DOWNLOAD_COMPLETE.store(true, Ordering::Relaxed);
    DOWNLOAD_IN_PROGRESS.store(false, Ordering::Relaxed);

    Ok(())
}

/// Ensure required data files (areas.json, channels.json, webui)
async fn ensure_required_files() -> Result<(), Box<dyn Error>> {
    let exe_dir = current_exe_dir()?;

    // Make the UI available before any remote data/dependency download.
    let installed = crate::webui::assets::install_missing_assets(&exe_dir)?;
    DOWNLOAD_PROGRESS.fetch_add(installed, Ordering::Relaxed);

    let initialized = initialize_user_data(&exe_dir).await?;
    DOWNLOAD_PROGRESS.fetch_add(initialized, Ordering::Relaxed);
    Ok(())
}

/// New installations start with user-owned data, never the developer's roster.
pub(crate) async fn initialize_user_data(_directory: &Path) -> Result<usize, String> {
    tokio::task::spawn_blocking(crate::storage::global)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(0)
}

/// Ensure Windows-specific dependencies (yt-dlp, ffmpeg)
#[cfg(target_os = "windows")]
async fn ensure_windows_dependencies() -> Result<(), Box<dyn Error>> {
    let exe_dir = current_exe_dir()?;

    println!("🔍 检查 Windows 依赖项...");

    // Check and download yt-dlp
    let yt_dlp_path = exe_dir.join("yt-dlp.exe");
    if !yt_dlp_path.exists() {
        println!("📥 下载 yt-dlp.exe...");
        download_file_to_path(YT_DLP_URL, &yt_dlp_path).await?;
        println!("✅ yt-dlp.exe 下载完成");
    } else {
        println!("✅ yt-dlp.exe 已存在");
    }

    // Check and download ffmpeg
    let ffmpeg_path = exe_dir.join("ffmpeg.exe");
    if !ffmpeg_path.exists() {
        println!("📥 下载 ffmpeg.exe (这可能需要几分钟)...");
        download_and_extract_ffmpeg(&exe_dir).await?;
        println!("✅ ffmpeg.exe 下载完成");
    } else {
        println!("✅ ffmpeg.exe 已存在");
    }

    // Check for streamlink (needs to be installed separately)
    if !check_streamlink_installed() {
        println!("⚠️  streamlink 未安装");
        println!("   对于 Twitch 支持，请安装 streamlink:");
        println!("   1. 下载: https://github.com/streamlink/windows-builds/releases");
        println!("   2. 或使用: pip install streamlink");
        println!("   3. 安装 ttvlol 插件: https://github.com/2bc4/streamlink-ttvlol");
        println!();
    } else {
        println!("✅ streamlink 已安装");
    }

    // Check for Deno (required by yt-dlp)
    if !check_deno_installed() {
        println!("⚠️  Deno 未安装");
        println!("   yt-dlp 需要 Deno 获取m3u8");
        println!("   正在自动安装 Deno...");

        match install_deno_windows().await {
            Ok(_) => {
                println!("✅ Deno 安装成功");
                println!("   请重启程序以使 Deno 生效");
            }
            Err(e) => {
                println!("❌ Deno 自动安装失败: {}", e);
                println!("   请手动安装:");
                println!("   PowerShell: irm https://deno.land/install.ps1 | iex");
            }
        }
        println!();
    } else {
        println!("✅ Deno 已安装");
    }

    println!("✅ 核心依赖项已就绪\n");
    Ok(())
}

#[cfg(target_os = "windows")]
fn check_streamlink_installed() -> bool {
    // Check if streamlink is in PATH
    std::process::Command::new("streamlink")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn check_deno_installed() -> bool {
    // Check if deno is in PATH
    std::process::Command::new("deno")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
async fn install_deno_windows() -> Result<(), Box<dyn Error>> {
    use std::io::Write;

    println!("📥 下载 Deno 安装脚本...");

    // Download the Deno install script
    let install_script_url = "https://deno.land/install.ps1";
    let response = reqwest::get(install_script_url).await?;
    let script_content = response.text().await?;

    // Save script to temp file
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join("install_deno.ps1");
    let mut file = fs::File::create(&script_path)?;
    file.write_all(script_content.as_bytes())?;
    drop(file);

    println!("🔧 运行安装脚本...");

    // Run PowerShell script
    let output = std::process::Command::new("powershell")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .output()?;

    // Clean up temp file
    let _ = fs::remove_file(&script_path);

    if output.status.success() {
        println!("📝 安装输出:");
        println!("{}", String::from_utf8_lossy(&output.stdout));
        Ok(())
    } else {
        Err(format!("安装失败: {}", String::from_utf8_lossy(&output.stderr)).into())
    }
}

/// Ensure Linux-specific dependencies (yt-dlp, ffmpeg, streamlink, deno)
#[cfg(not(target_os = "windows"))]
async fn ensure_linux_dependencies() -> Result<(), Box<dyn Error>> {
    println!("🔍 检查 Linux 依赖项...");

    // Check for yt-dlp
    if !check_command_installed("yt-dlp") {
        println!("⚠️  yt-dlp 未安装");
        println!("   安装方法:");
        println!("   sudo curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp");
        println!("   sudo chmod a+rx /usr/local/bin/yt-dlp");
        println!("   或使用: pip install yt-dlp");
        println!();
    } else {
        println!("✅ yt-dlp 已安装");
    }

    // Check for ffmpeg
    if !check_command_installed("ffmpeg") {
        println!("⚠️  ffmpeg 未安装");
        println!("   安装方法:");
        println!("   Ubuntu/Debian: sudo apt install ffmpeg");
        println!("   Fedora: sudo dnf install ffmpeg");
        println!("   Arch: sudo pacman -S ffmpeg");
        println!();
    } else {
        println!("✅ ffmpeg 已安装");
    }

    // Check for streamlink
    if !check_command_installed("streamlink") {
        println!("⚠️  streamlink 未安装");
        println!("   对于 Twitch 支持，请安装 streamlink:");
        println!("   pip install streamlink");
        println!("   安装 ttvlol 插件: https://github.com/2bc4/streamlink-ttvlol");
        println!();
    } else {
        println!("✅ streamlink 已安装");
    }

    // Check for Deno
    if !check_command_installed("deno") {
        println!("⚠️  Deno 未安装");
        println!("   yt-dlp 需要 Deno 来处理某些网站（如 YouTube）");
        println!("   安装方法:");
        println!("   curl -fsSL https://deno.land/install.sh | sh");
        println!("   然后将 Deno 添加到 PATH:");
        println!("   export PATH=\"$HOME/.deno/bin:$PATH\"");
        println!();
    } else {
        println!("✅ Deno 已安装");
    }

    println!("✅ 依赖项检查完成\n");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn check_command_installed(command: &str) -> bool {
    std::process::Command::new(command)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
async fn download_file_to_path(url: &str, dest: &PathBuf) -> Result<(), Box<dyn Error>> {
    let response = reqwest::get(url).await?;
    let bytes = response.bytes().await?;

    let mut file = fs::File::create(dest)?;
    file.write_all(&bytes)?;

    Ok(())
}

#[cfg(target_os = "windows")]
async fn download_and_extract_ffmpeg(dest_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Download the zip file
    let response = reqwest::get(FFMPEG_URL).await?;
    let bytes = response.bytes().await?;

    // Save to temporary file
    let temp_zip = dest_dir.join("ffmpeg_temp.zip");
    let mut file = fs::File::create(&temp_zip)?;
    file.write_all(&bytes)?;
    drop(file);

    // Extract ffmpeg.exe from the zip
    let file = fs::File::open(&temp_zip)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Find and extract ffmpeg.exe
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let file_name = file.name();

        if file_name.ends_with("ffmpeg.exe") && !file_name.contains("..") {
            let dest_path = dest_dir.join("ffmpeg.exe");
            let mut outfile = fs::File::create(&dest_path)?;
            std::io::copy(&mut file, &mut outfile)?;
            break;
        }
    }

    // Clean up temp file
    let _ = fs::remove_file(&temp_zip);

    Ok(())
}

pub fn check_files_exist() -> bool {
    let Ok(exe_dir) = current_exe_dir() else {
        return false;
    };

    let webui_index = exe_dir.join("webui").join("dist").join("index.html");

    crate::storage::contains("areas.json").unwrap_or(false)
        && crate::storage::contains("channels.json").unwrap_or(false)
        && webui_index.exists()
}

fn current_exe_dir() -> Result<PathBuf, Box<dyn Error>> {
    let exe = std::env::current_exe()?;
    executable_parent_dir(&exe)
}

fn executable_parent_dir(exe: &Path) -> Result<PathBuf, Box<dyn Error>> {
    exe.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("Failed to get executable directory: {}", exe.display()).into())
}

fn recover_lock<T>(lock: LockResult<T>, name: &str) -> T {
    lock.unwrap_or_else(|poisoned| {
        tracing::warn!("Recovering poisoned {}", name);
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn zip_archives_round_trip_deflate() {
        use std::io::{Cursor, Read, Write};
        use zip::write::SimpleFileOptions;

        let body = "ffmpeg".repeat(512);
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buffer);
        writer
            .start_file(
                "ffmpeg-master/bin/ffmpeg.exe",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(body.as_bytes()).unwrap();
        writer.finish().unwrap();

        let compressed = buffer.into_inner();
        assert!(
            compressed.len() < body.len(),
            "entry was stored, not deflated, so this would not exercise the backend"
        );

        let mut archive = zip::ZipArchive::new(Cursor::new(compressed)).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        assert_eq!(entry.name(), "ffmpeg-master/bin/ffmpeg.exe");
        let mut extracted = String::new();
        entry.read_to_string(&mut extracted).unwrap();
        assert_eq!(extracted, body);
    }
}
