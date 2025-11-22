use std::error::Error;
use std::fs;

#[cfg(target_os = "windows")]
use std::io::Write;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

const GITHUB_RAW_BASE: &str = "https://raw.githubusercontent.com/Detteee/bilistream/main";

#[cfg(target_os = "windows")]
const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
#[cfg(target_os = "windows")]
const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";

/// Ensure all required files and dependencies are present
pub async fn ensure_all_dependencies() -> Result<(), Box<dyn Error>> {
    // First, ensure required data files (cross-platform)
    ensure_required_files().await?;

    // Then, ensure platform-specific dependencies
    #[cfg(target_os = "windows")]
    ensure_windows_dependencies().await?;

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
