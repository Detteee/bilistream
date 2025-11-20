use std::fs;
use std::io::Write;
use std::path::PathBuf;

const YT_DLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";

pub async fn ensure_dependencies() -> Result<(), Box<dyn std::error::Error>> {
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();

    println!("🔍 检查 Windows 依赖项...");

    // Check and download yt-dlp
    let yt_dlp_path = exe_dir.join("yt-dlp.exe");
    if !yt_dlp_path.exists() {
        println!("📥 下载 yt-dlp.exe...");
        download_file(YT_DLP_URL, &yt_dlp_path).await?;
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

fn check_streamlink_installed() -> bool {
    // Check if streamlink is in PATH
    std::process::Command::new("streamlink")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn download_file(url: &str, dest: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let response = reqwest::get(url).await?;
    let bytes = response.bytes().await?;

    let mut file = fs::File::create(dest)?;
    file.write_all(&bytes)?;

    Ok(())
}

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
