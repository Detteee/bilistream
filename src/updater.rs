use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_REPO: &str = "Detteee/bilistream";
const GITHUB_API_BASE: &str = "https://api.github.com/repos";

#[derive(Debug, Deserialize, Serialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub name: String,
    pub body: String,
    pub html_url: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub has_update: bool,
    pub download_url: Option<String>,
    pub release_notes: Option<String>,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
}

/// Check if a new version is available
pub async fn check_for_updates() -> Result<UpdateInfo, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .user_agent("bilistream")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let url = format!("{}/{}/releases/latest", GITHUB_API_BASE, GITHUB_REPO);
    let response = client.get(&url).send().await?;

    if !response.status().is_success() {
        return Err(format!("GitHub API 请求失败: {}", response.status()).into());
    }

    let release: ReleaseInfo = response.json().await?;
    let latest_version = release.tag_name.trim_start_matches('v');
    let has_update = compare_versions(latest_version, CURRENT_VERSION) > 0;

    // Determine the appropriate asset for the current platform
    let (asset_name, download_url, asset_size) = if has_update {
        get_platform_asset(&release.assets)?
    } else {
        (None, None, None)
    };

    Ok(UpdateInfo {
        current_version: CURRENT_VERSION.to_string(),
        latest_version: latest_version.to_string(),
        has_update,
        download_url,
        release_notes: Some(release.body),
        asset_name,
        asset_size,
    })
}

/// Get the appropriate download asset for the current platform
fn get_platform_asset(
    assets: &[ReleaseAsset],
) -> Result<(Option<String>, Option<String>, Option<u64>), Box<dyn Error + Send + Sync>> {
    let platform_suffix = if cfg!(target_os = "windows") {
        "_for_windows.zip"
    } else if cfg!(target_os = "linux") {
        "_for_linux.tar.gz"
    } else if cfg!(target_os = "macos") {
        "_for_macos.tar.gz"
    } else {
        return Ok((None, None, None));
    };

    // Find the asset that matches the platform
    for asset in assets {
        if asset.name.ends_with(platform_suffix) || asset.name.contains(platform_suffix) {
            return Ok((
                Some(asset.name.clone()),
                Some(asset.browser_download_url.clone()),
                Some(asset.size),
            ));
        }
    }

    // Fallback: try to find by platform keywords
    let platform_keyword = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "macos"
    };

    for asset in assets {
        if asset.name.to_lowercase().contains(platform_keyword) {
            return Ok((
                Some(asset.name.clone()),
                Some(asset.browser_download_url.clone()),
                Some(asset.size),
            ));
        }
    }

    Ok((None, None, None))
}

/// Download and install an update
pub async fn download_and_install_update(
    download_url: &str,
    _progress_callback: Option<Box<dyn Fn(u64, u64) + Send>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing::info!("📥 开始下载更新: {}", download_url);

    let client = reqwest::Client::builder()
        .user_agent("bilistream")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let response = client.get(download_url).send().await?;

    if !response.status().is_success() {
        return Err(format!("下载失败: HTTP {}", response.status()).into());
    }

    let total_size = response.content_length().unwrap_or(0);

    // Create temp directory
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or("无法获取可执行文件目录")?
        .to_path_buf();
    let temp_dir = exe_dir.join(".update_temp");
    fs::create_dir_all(&temp_dir)?;

    // Determine file extension
    let file_ext = if download_url.ends_with(".zip") {
        "zip"
    } else if download_url.ends_with(".tar.gz") {
        "tar.gz"
    } else {
        "bin"
    };

    let temp_file = temp_dir.join(format!("update.{}", file_ext));
    let mut file = fs::File::create(&temp_file)?;

    // Download with progress
    use std::io::Write;

    tracing::info!("📥 下载中... (大小: {} MB)", total_size / 1024 / 1024);
    let bytes = response.bytes().await?;
    file.write_all(&bytes)?;
    let downloaded = bytes.len() as u64;

    tracing::info!("✅ 下载完成: {} bytes", downloaded);

    file.sync_all()?;
    drop(file);

    tracing::info!("✅ 下载完成，开始更新...");

    // Extract and install
    install_update(&temp_file, &exe_dir)?;

    // Clean up
    let _ = fs::remove_dir_all(&temp_dir);

    tracing::info!("✅ 更新完成！");
    tracing::info!("⚠️  请重启程序以使用新版本");

    Ok(())
}

/// Install the downloaded update
fn install_update(
    archive_path: &PathBuf,
    install_dir: &PathBuf,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    #[cfg(target_os = "windows")]
    {
        install_windows_update(archive_path, install_dir)?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        install_unix_update(archive_path, install_dir)?;
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn install_windows_update(
    archive_path: &PathBuf,
    install_dir: &PathBuf,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Extract zip file
    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    // Backup current executable
    let current_exe = std::env::current_exe()?;
    let backup_exe = current_exe.with_extension("exe.old");
    let _ = fs::rename(&current_exe, &backup_exe);

    // Extract files from archive
    // Release structure: bilistream_for_windows/
    //   ├── bilistream.exe
    //   ├── README.md
    //   ├── README.zh_CN.md
    //   └── webui/dist/index.html

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let file_path = file.name().to_string(); // Convert to owned String

        // Skip directories
        if file_path.ends_with('/') {
            continue;
        }

        // Get the relative path (remove the archive root folder)
        let relative_path = if let Some(pos) = file_path.find('/') {
            file_path[pos + 1..].to_string()
        } else {
            file_path.clone()
        };

        // Skip if empty (root folder itself)
        if relative_path.is_empty() {
            continue;
        }

        let dest_path = install_dir.join(&relative_path);

        // Create parent directories if needed
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Extract file
        let mut outfile = fs::File::create(&dest_path)?;
        std::io::copy(&mut file, &mut outfile)?;

        tracing::info!("✅ 已更新: {}", relative_path);
    }

    // Create a batch script to restart the program
    let restart_script = install_dir.join("restart_after_update.bat");
    let script_content = format!(
        r#"@echo off
timeout /t 2 /nobreak >nul
start "" "{}"
del "%~f0"
"#,
        current_exe.display()
    );
    fs::write(&restart_script, script_content)?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn install_unix_update(
    archive_path: &PathBuf,
    install_dir: &PathBuf,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    use std::process::Command;

    // Create temp extraction directory
    let temp_extract = install_dir.join(".update_extract");
    fs::create_dir_all(&temp_extract)?;

    // Extract tar.gz to temp directory
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(archive_path)
        .arg("-C")
        .arg(&temp_extract)
        .output()?;

    if !output.status.success() {
        let _ = fs::remove_dir_all(&temp_extract);
        return Err("解压失败".into());
    }

    // Find the extracted directory (should be bilistream_for_linux/)
    let extracted_dir = fs::read_dir(&temp_extract)?
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .ok_or("找不到解压的目录")?
        .path();

    // Backup current executable
    let current_exe = std::env::current_exe()?;
    let backup_exe = current_exe.with_extension("old");
    let _ = fs::rename(&current_exe, &backup_exe);

    // Copy files from extracted directory to install directory
    // Release structure: bilistream_for_linux/
    //   ├── bilistream
    //   ├── README.md
    //   ├── README.zh_CN.md
    //   └── webui/dist/index.html

    copy_dir_recursive(&extracted_dir, install_dir)?;

    // Make executable
    let new_exe = install_dir.join("bilistream");
    Command::new("chmod").arg("+x").arg(&new_exe).output()?;

    // Clean up temp directory
    let _ = fs::remove_dir_all(&temp_extract);

    tracing::info!("✅ 已更新: bilistream");

    Ok(())
}

// Helper function to recursively copy directory contents
#[cfg(not(target_os = "windows"))]
fn copy_dir_recursive(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
            tracing::info!("✅ 已更新: {}", entry.file_name().to_string_lossy());
        }
    }
    Ok(())
}

fn compare_versions(v1: &str, v2: &str) -> i32 {
    let parts1: Vec<u32> = v1.split('.').filter_map(|s| s.parse().ok()).collect();
    let parts2: Vec<u32> = v2.split('.').filter_map(|s| s.parse().ok()).collect();

    for i in 0..std::cmp::max(parts1.len(), parts2.len()) {
        let part1 = parts1.get(i).copied().unwrap_or(0);
        let part2 = parts2.get(i).copied().unwrap_or(0);

        if part1 > part2 {
            return 1;
        }
        if part1 < part2 {
            return -1;
        }
    }

    0
}
