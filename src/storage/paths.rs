use super::crypto::{random, Key};
#[cfg(not(test))]
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn executable_dir() -> io::Result<PathBuf> {
    std::env::current_exe()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("无法定位程序目录"))
}

#[cfg(not(test))]
pub(super) fn global_paths() -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let legacy = executable_dir()?;
    let data = std::env::var_os("BILISTREAM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| legacy.join("data"));
    let data = std::path::absolute(data)?;
    let key = if let Some(path) = std::env::var_os("BILISTREAM_KEY_FILE") {
        std::path::absolute(path)?
    } else {
        let root = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        }
        .ok_or_else(|| io::Error::other("请设置 BILISTREAM_KEY_FILE 指定独立密钥文件"))?;
        let id = hex(&Sha256::digest(data.to_string_lossy().as_bytes()));
        root.join("bilistream")
            .join("keys")
            .join(format!("{id}.key"))
    };
    if key.starts_with(&data) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "密钥文件必须保存在数据库目录之外",
        ));
    }
    Ok((data, key, legacy))
}

pub fn private_dir(path: &Path) -> io::Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "私有数据目录不能是符号链接",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        fs::create_dir_all(path)?;
        let user =
            std::env::var("USERNAME").map_err(|_| io::Error::other("无法确认 Windows 运行账号"))?;
        let mut cmd = std::process::Command::new("icacls");
        crate::plugins::utils::configure_no_window(&mut cmd);
        let output = cmd
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("{user}:(OI)(CI)F")])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "无法限制私有数据目录权限",
            ));
        }
    }
    Ok(())
}

pub fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_private(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    // The verified recovery/key must survive a crash before plaintext retires.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn load_key(path: &Path, existing: bool) -> io::Result<Key> {
    if !path.try_exists()? {
        if existing {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "数据库密钥缺失，请恢复原密钥或导入加密备份",
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("密钥路径无效"))?;
        private_dir(parent)?;
        let key = Key(random()?);
        let encoded = protect(&key.0)?;
        write_private(path, &encoded)?;
        return Ok(key);
    }
    let metadata = path.symlink_metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "密钥文件无效"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "密钥权限不安全，需要仅运行账号可读写（600）",
            ));
        }
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(16 * 1024).read_to_end(&mut bytes)?;
    let plain = unprotect(&bytes)?;
    Ok(Key(plain.as_slice().try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "密钥长度无效")
    })?))
}

#[cfg(not(windows))]
fn protect(bytes: &[u8]) -> io::Result<Vec<u8>> {
    Ok(bytes.to_vec())
}
#[cfg(not(windows))]
fn unprotect(bytes: &[u8]) -> io::Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(windows)]
fn dpapi(bytes: &[u8], encrypt: bool) -> io::Result<Vec<u8>> {
    use std::ptr;
    use winapi::um::dpapi::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN};
    use winapi::um::winbase::LocalFree;
    use winapi::um::wincrypt::DATA_BLOB;
    let mut input = DATA_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = DATA_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &mut input,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &mut input,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if success == 0 {
        return Err(io::Error::other("当前 Windows 账号无法读取存储密钥"));
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(result)
}
#[cfg(windows)]
fn protect(bytes: &[u8]) -> io::Result<Vec<u8>> {
    dpapi(bytes, true)
}
#[cfg(windows)]
fn unprotect(bytes: &[u8]) -> io::Result<Vec<u8>> {
    dpapi(bytes, false)
}
