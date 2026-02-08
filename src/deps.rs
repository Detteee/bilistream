use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(target_os = "windows")]
use std::io::Write;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

const GITHUB_RAW_BASE: &str = "https://raw.githubusercontent.com/Detteee/bilistream/main";

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
    let message = DOWNLOAD_MESSAGE.lock().unwrap().clone();
    (progress, total, message)
}

fn set_download_message(msg: &str) {
    *DOWNLOAD_MESSAGE.lock().unwrap() = msg.to_string();
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
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or("Failed to get executable directory")?
        .to_path_buf();

    if !exe_dir.join("areas.json").exists() {
        total_items += 1;
    }
    if !exe_dir.join("channels.json").exists() {
        total_items += 1;
    }
    if !exe_dir
        .join("webui")
        .join("dist")
        .join("index.html")
        .exists()
    {
        total_items += 1;
    }

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

    set_download_message(&format!("开始下载 {} 个文件...", total_items));

    // First, ensure required data files (cross-platform)
    ensure_required_files().await?;

    // Then, ensure platform-specific dependencies
    #[cfg(target_os = "windows")]
    ensure_windows_dependencies().await?;

    #[cfg(not(target_os = "windows"))]
    ensure_linux_dependencies().await?;

    set_download_message("所有依赖下载完成！");
    DOWNLOAD_COMPLETE.store(true, Ordering::Relaxed);
    DOWNLOAD_IN_PROGRESS.store(false, Ordering::Relaxed);

    Ok(())
}

/// Ensure required data files (areas.json, channels.json, webui)
async fn ensure_required_files() -> Result<(), Box<dyn Error>> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or("Failed to get executable directory")?
        .to_path_buf();

    let mut missing_files = Vec::new();

    // Check for required files
    let areas_json = exe_dir.join("areas.json");
    let channels_json = exe_dir.join("channels.json");
    let webui_index = exe_dir.join("webui").join("dist").join("index.html");

    if !areas_json.exists() {
        missing_files.push(("areas.json", "areas.json"));
    }
    if !channels_json.exists() {
        missing_files.push(("channels.json", "channels.json"));
    }
    if !webui_index.exists() {
        missing_files.push(("webui/dist/index.html", "webui/dist/index.html"));
    }

    if missing_files.is_empty() {
        return Ok(());
    }

    println!("\n📦 检测到缺少必需文件，正在自动下载...");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    for (local_path, remote_path) in missing_files {
        println!("⬇️  下载: {}", local_path);

        let url = format!("{}/{}", GITHUB_RAW_BASE, remote_path);
        let content = download_file_bytes(&url).await?;

        let full_path = exe_dir.join(local_path);

        // Create parent directories if needed
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&full_path, content)?;
        println!("✅ 已保存: {}", local_path);
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("✅ 所有必需文件已下载完成\n");

    // Show file usage information
    show_file_usage_info();

    Ok(())
}

/// Ensure Windows-specific dependencies (yt-dlp, ffmpeg)
#[cfg(target_os = "windows")]
async fn ensure_windows_dependencies() -> Result<(), Box<dyn Error>> {
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();

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

    // Check and download ImageMagick (convert.exe)
    let convert_path = exe_dir.join("convert.exe");
    if !convert_path.exists() {
        println!("📥 下载 ImageMagick...");
        match download_imagemagick(&exe_dir).await {
            Ok(_) => println!("✅ ImageMagick 下载完成"),
            Err(e) => {
                println!("⚠️  ImageMagick 下载失败: {}", e);
                println!("   封面功能可能无法使用");
                println!("   手动下载: https://github.com/ImageMagick/ImageMagick/releases/latest");
                println!("   下载 ImageMagick-*-portable-Q16-HDRI-x64.7z");
                println!("   解压后将 magick.exe 重命名为 convert.exe 并放到程序目录");
            }
        }
        println!();
    } else {
        println!("✅ ImageMagick 已存在");
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

async fn download_file_bytes(url: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let client = reqwest::Client::builder()
        .user_agent("bilistream")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Err(format!("下载失败: HTTP {}", response.status()).into());
    }

    let bytes = response.bytes().await?;
    Ok(bytes.to_vec())
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

#[cfg(target_os = "windows")]
async fn download_imagemagick(dest_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    // Fetch the latest release from GitHub API
    let client = reqwest::Client::builder()
        .user_agent("bilistream")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    println!("🔍 获取最新版本信息...");
    let releases_url = "https://api.github.com/repos/ImageMagick/ImageMagick/releases/latest";
    let release_response = client.get(releases_url).send().await?;

    if !release_response.status().is_success() {
        return Err(format!("获取版本信息失败: HTTP {}", release_response.status()).into());
    }

    let release_data: serde_json::Value = release_response.json().await?;
    let assets = release_data["assets"].as_array().ok_or("无法解析 assets")?;

    // Find the portable-Q16-HDRI-x64.7z file
    let asset = assets
        .iter()
        .find(|a| {
            if let Some(name) = a["name"].as_str() {
                name.contains("portable")
                    && name.contains("Q16-HDRI")
                    && name.contains("x64")
                    && name.ends_with(".7z")
            } else {
                false
            }
        })
        .ok_or("未找到合适的 ImageMagick 版本")?;

    let download_url = asset["browser_download_url"]
        .as_str()
        .ok_or("无法获取下载链接")?;

    let file_name = asset["name"].as_str().unwrap_or("ImageMagick.7z");

    println!("📥 下载 {}...", file_name);

    let response = client.get(download_url).send().await?;

    if !response.status().is_success() {
        return Err(format!("下载失败: HTTP {}", response.status()).into());
    }

    let bytes = response.bytes().await?;

    println!("📦 下载完成，大小: {} MB", bytes.len() / 1024 / 1024);

    if bytes.len() < 100000 {
        return Err("下载的文件太小，可能是错误页面".into());
    }

    // Save to temporary file
    let temp_7z = dest_dir.join("imagemagick_temp.7z");
    let mut file = fs::File::create(&temp_7z)?;
    file.write_all(&bytes)?;
    drop(file);

    println!("📂 正在解压 magick.exe...");

    // Extract only magick.exe from the 7z archive
    let temp_extract_dir = dest_dir.join("imagemagick_temp");
    fs::create_dir_all(&temp_extract_dir)?;

    sevenz_rust::decompress_file(&temp_7z, &temp_extract_dir)
        .map_err(|e| format!("解压失败: {}", e))?;

    // Find magick.exe in the extracted files
    let mut magick_found = false;
    for entry in fs::read_dir(&temp_extract_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some("magick.exe") {
            let convert_path = dest_dir.join("convert.exe");
            fs::rename(&path, &convert_path)?;
            println!("✅ 已将 magick.exe 重命名为 convert.exe");
            magick_found = true;
            break;
        }
    }

    // Clean up temp files
    let _ = fs::remove_file(&temp_7z);
    let _ = fs::remove_dir_all(&temp_extract_dir);

    if !magick_found {
        return Err("未找到 magick.exe".into());
    }

    // Clean up temp file
    let _ = fs::remove_file(&temp_7z);

    Ok(())
}

fn show_file_usage_info() {
    println!("\n📚 文件说明：");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    println!("\n📄 areas.json");
    println!("   用途: 定义 B 站直播分区、禁用关键词和智能分区匹配");
    println!("   包含:");
    println!("   • banned_keywords: 标题中包含这些词的直播将被跳过");
    println!("   • areas: B 站直播分区配置");
    println!("     - id: 分区 ID");
    println!("     - name: 分区名称");
    println!("     - title_keywords: 标题关键词（自动匹配分区）");
    println!("     - aliases: 弹幕指令别名");
    println!("   示例:");
    println!("   • 添加禁用词: 在 banned_keywords 中添加 'chat'");
    println!("   • 智能分区: 标题包含 'valorant' 自动选择无畏契约分区");
    println!("   • 弹幕指令: 发送 '%转播%YT%频道%lol' 使用别名 'lol' 选择英雄联盟");

    println!("\n📄 channels.json");
    println!("   用途: 预设的 YouTube/Twitch 频道列表");
    println!("   包含:");
    println!("   • name: 频道名称");
    println!("   • platforms: YouTube 频道 ID 和 Twitch 用户名");
    println!("   • riot_puuid: 英雄联盟玩家 ID（用于 LOL 监控）");
    println!("   示例: 在 Web UI 中选择频道时会显示这些预设选项");

    println!("\n📄 webui/dist/index.html");
    println!("   用途: Web 控制面板界面");
    println!("   功能:");
    println!("   • 首次运行设置向导");
    println!("   • 实时监控直播状态");
    println!("   • 控制开播/停播");
    println!("   • 管理频道和配置");

    println!("\n💡 提示:");
    println!("   • 可以编辑 areas.json 添加自定义禁用关键词");
    println!("   • 可以编辑 channels.json 添加常用频道");
    println!("   • 这些文件会在程序启动时自动加载");

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
}

pub fn check_files_exist() -> bool {
    let exe_dir = match std::env::current_exe() {
        Ok(path) => match path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => return false,
        },
        Err(_) => return false,
    };

    let areas_json = exe_dir.join("areas.json");
    let channels_json = exe_dir.join("channels.json");
    let webui_index = exe_dir.join("webui").join("dist").join("index.html");

    areas_json.exists() && channels_json.exists() && webui_index.exists()
}
