use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

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
    let asset = has_update
        .then(|| get_platform_asset(&release.assets))
        .flatten();
    Ok(UpdateInfo {
        current_version: CURRENT_VERSION.to_string(),
        latest_version: latest_version.to_string(),
        has_update,
        download_url: asset.map(|asset| asset.browser_download_url.clone()),
        release_notes: Some(release.body),
        asset_name: asset.map(|asset| asset.name.clone()),
        asset_size: asset.map(|asset| asset.size),
    })
}

/// Get the appropriate download asset for the current platform
fn get_platform_asset(assets: &[ReleaseAsset]) -> Option<&ReleaseAsset> {
    // Tauri build gets the tauri-specific archive; regular build gets the standard one
    let platform_suffix = if cfg!(feature = "tauri-build") {
        if cfg!(target_os = "windows") {
            "_windows.zip"
        } else {
            "_linux.tar.gz"
        }
    } else if cfg!(target_os = "windows") {
        "_windows.zip"
    } else if cfg!(target_os = "linux") {
        "_linux.tar.gz"
    } else if cfg!(target_os = "macos") {
        "_for_macos.tar.gz"
    } else {
        return None;
    };

    // Find the asset that matches the platform
    for asset in assets {
        if asset.name.ends_with(platform_suffix) || asset.name.contains(platform_suffix) {
            return Some(asset);
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

    assets
        .iter()
        .find(|asset| asset.name.to_lowercase().contains(platform_keyword))
}

#[derive(Clone, Serialize)]
pub struct UpdateStatus {
    pub phase: &'static str,
    pub message: String,
}
static UPDATE_STATUS: std::sync::LazyLock<std::sync::Mutex<UpdateStatus>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(UpdateStatus {
            phase: "idle",
            message: String::new(),
        })
    });
pub fn update_status() -> UpdateStatus {
    UPDATE_STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
pub fn begin_update() -> bool {
    let mut status = UPDATE_STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(status.phase, "downloading" | "installing" | "restarting") {
        return false;
    }
    *status = UpdateStatus {
        phase: "downloading",
        message: "正在下载更新".into(),
    };
    true
}
pub fn set_update_status(phase: &'static str, message: impl Into<String>) {
    *UPDATE_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = UpdateStatus {
        phase,
        message: message.into(),
    };
}

// Only program assets and reader-facing guides; never runtime data or keys.
fn should_update_file(relative_path: &str, _install_dir: &Path) -> bool {
    let name = relative_path.replace('\\', "/");
    if name.split('/').any(|part| part == ".." || part.is_empty()) {
        return false;
    }
    matches!(
        name.as_str(),
        "bilistream"
            | "bilistream.exe"
            | "bilistream-tauri"
            | "bilistream-tauri.exe"
            | "README.md"
            | "README.zh_CN.md"
    ) || name.starts_with("webui/dist/")
        || name.starts_with("webui/public-dist/")
        || name.starts_with("docs/")
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
    let bytes = crate::plugins::http::response_bytes_limited(response, 256 * 1024 * 1024).await?;
    file.write_all(&bytes)?;
    let downloaded = bytes.len() as u64;

    tracing::info!("✅ 下载完成: {} bytes", downloaded);

    file.sync_all()?;
    drop(file);

    tracing::info!("✅ 下载完成，开始更新...");

    set_update_status("installing", "正在安装更新");
    tokio::task::spawn_blocking(move || install_update(&temp_file, &exe_dir)).await??;

    // Clean up
    let _ = fs::remove_dir_all(&temp_dir);

    tracing::info!("✅ 更新完成！");
    tracing::info!("⚠️  请重启程序以使用新版本");

    Ok(())
}

/// Stage a complete package before replacing any installed file.
fn install_update(
    archive_path: &PathBuf,
    install_dir: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let extract = install_dir.join(".update_extract");
    if extract.exists() {
        fs::remove_dir_all(&extract)?;
    }
    fs::create_dir(&extract)?;
    #[cfg(target_os = "windows")]
    {
        let mut archive = zip::ZipArchive::new(fs::File::open(archive_path)?)?;
        archive.extract(&extract)?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(archive_path)
            .arg("-C")
            .arg(&extract)
            .args(["--no-same-owner", "--no-same-permissions"])
            .output()?;
        if !output.status.success() {
            return Err("解压更新包失败".into());
        }
    }
    let root = extract.join(if cfg!(windows) {
        "bilistream_windows"
    } else {
        "bilistream_linux"
    });
    let binary = if cfg!(feature = "tauri-build") {
        if cfg!(windows) {
            "bilistream-tauri.exe"
        } else {
            "bilistream-tauri"
        }
    } else if cfg!(windows) {
        "bilistream.exe"
    } else {
        "bilistream"
    };
    install_staged_update(&root, install_dir, binary)?;
    fs::remove_dir_all(&extract)?;
    Ok(())
}

fn validate_update_tree(path: &Path) -> Result<(), Box<dyn Error + Send + Sync>> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() || !(kind.is_dir() || kind.is_file()) {
            return Err("更新包包含无效文件".into());
        }
        if kind.is_dir() {
            validate_update_tree(&entry.path())?;
        }
    }
    Ok(())
}

fn install_staged_update(
    root: &Path,
    install_dir: &Path,
    binary: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    validate_update_tree(root)?;
    for required in [
        binary,
        "webui/dist/index.html",
        "webui/dist/js/main.js",
        "webui/public-dist/index.html",
    ] {
        if !root.join(required).is_file() || fs::metadata(root.join(required))?.len() == 0 {
            return Err(format!("更新包不完整，缺少 {required}").into());
        }
    }
    #[cfg(unix)]
    for name in ["bilistream", "bilistream-tauri"] {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        if path.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
    }
    let mut installed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let result = (|| -> Result<(), Box<dyn Error + Send + Sync>> {
        // Replace whole UI trees; removed modules cannot linger after upgrades.
        for name in [
            "webui/dist",
            "webui/public-dist",
            "README.md",
            "README.zh_CN.md",
            "docs",
            "bilistream",
            "bilistream.exe",
            "bilistream-tauri",
            "bilistream-tauri.exe",
        ] {
            let source = root.join(name);
            if !source.exists() {
                continue;
            }
            if source.is_file() && !should_update_file(name, install_dir) {
                continue;
            }
            let destination = install_dir.join(name);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let backup = PathBuf::from(format!("{}.old", destination.display()));
            if backup.is_dir() {
                fs::remove_dir_all(&backup)?;
            } else if backup.exists() {
                fs::remove_file(&backup)?;
            }
            let previous = if destination.exists() {
                fs::rename(&destination, &backup)?;
                Some(backup)
            } else {
                None
            };
            installed.push((destination.clone(), previous));
            fs::rename(source, destination)?;
        }
        Ok(())
    })();
    if result.is_err() {
        for (destination, backup) in installed.into_iter().rev() {
            if destination.is_dir() {
                let _ = fs::remove_dir_all(&destination);
            } else if destination.exists() {
                let _ = fs::remove_file(&destination);
            }
            if let Some(backup) = backup {
                fs::rename(backup, destination)?;
            }
        }
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut bytes = [0u8; 12];
            getrandom::fill(&mut bytes).unwrap();
            let root = std::env::temp_dir().join(format!(
                "bilistream-update-test-{:x}",
                u128::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                    bytes[8], bytes[9], bytes[10], bytes[11], 0, 0, 0, 0
                ])
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn package(&self) -> PathBuf {
            let root = self.0.join("stage");
            for name in [
                "bilistream",
                "webui/dist/index.html",
                "webui/dist/js/main.js",
                "webui/public-dist/index.html",
            ] {
                let path = root.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"new").unwrap();
            }
            root
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn staged_update_replaces_assets_and_preserves_all_runtime_data() {
        let fixture = Fixture::new();
        let package = fixture.package();
        let target = fixture.0.join("installed");
        fs::create_dir_all(target.join("webui/dist")).unwrap();
        fs::create_dir_all(target.join("data")).unwrap();
        fs::write(target.join("bilistream"), b"old binary").unwrap();
        fs::write(target.join("webui/dist/obsolete.js"), b"old module").unwrap();
        for name in [
            "config.json",
            "cookies.txt",
            "data/bilistream.db",
            "data/bilistream.db-wal",
        ] {
            fs::write(target.join(name), b"preserved").unwrap();
        }
        fs::write(package.join("config.json"), b"must not install").unwrap();
        install_staged_update(&package, &target, "bilistream").unwrap();
        assert_eq!(fs::read(target.join("bilistream")).unwrap(), b"new");
        assert_eq!(
            fs::read(target.join("bilistream.old")).unwrap(),
            b"old binary"
        );
        assert!(!target.join("webui/dist/obsolete.js").exists());
        for name in [
            "config.json",
            "cookies.txt",
            "data/bilistream.db",
            "data/bilistream.db-wal",
        ] {
            assert_eq!(fs::read(target.join(name)).unwrap(), b"preserved");
        }
    }

    #[test]
    fn incomplete_package_never_moves_the_running_binary() {
        let fixture = Fixture::new();
        let package = fixture.package();
        let target = fixture.0.join("installed");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("bilistream"), b"old binary").unwrap();
        fs::remove_file(package.join("webui/dist/js/main.js")).unwrap();
        assert!(install_staged_update(&package, &target, "bilistream").is_err());
        assert_eq!(fs::read(target.join("bilistream")).unwrap(), b"old binary");
        assert!(!target.join("bilistream.old").exists());
    }

    #[test]
    fn always_updates_webui_dist_tree() {
        let dir = Path::new(".");
        assert!(should_update_file("webui/dist/index.html", dir));
        assert!(should_update_file("webui/dist/js/main.js", dir));
        assert!(should_update_file("webui/dist/js/api.js", dir));
        assert!(should_update_file("webui/dist/styles.css", dir));
        assert!(should_update_file("webui/public-dist/index.html", dir));
        assert!(should_update_file("webui/public-dist/js/main.js", dir));
        assert!(should_update_file(r"webui\public-dist\public.css", dir));
        assert!(should_update_file(r"webui\dist\js\cluster.js", dir));
        assert!(should_update_file("README.md", dir));
        assert!(!should_update_file("webui/dist/../../config.json", dir));
        assert!(!should_update_file("data/bilistream.db", dir));
        assert!(!should_update_file("config.json", dir));
    }
}
