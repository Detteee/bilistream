use std::path::{Path, PathBuf};

pub struct RestartSpec {
    pub cwd: PathBuf,
    pub exe: String,
    pub args: Vec<String>,
    credential: Option<RestartCredential>,
}

struct RestartCredential {
    path: PathBuf,
    keep: bool,
}
impl Drop for RestartCredential {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct RestartCommand {
    text: String,
    credential: Option<RestartCredential>,
}
impl RestartCommand {
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn handed_off(mut self) {
        if let Some(credential) = &mut self.credential {
            credential.keep = true;
        }
    }
}

const RESTART_PASSWORD_LIMIT: usize = 64 * 1024;

#[cfg(test)]
fn write_restart_password(directory: &Path, password: &str) -> Result<RestartCredential, String> {
    if password.is_empty() || password.len() > RESTART_PASSWORD_LIMIT {
        return Err("访问密码为空或过长，无法准备重启".into());
    }
    crate::storage::paths::private_dir(directory).map_err(|_| "无法创建重启凭据目录")?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "无法生成重启凭据标识")?;
    let name: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
    let credential = RestartCredential {
        path: directory.join(format!("auth-{name}.tmp")),
        keep: false,
    };
    crate::storage::paths::write_private(&credential.path, password.as_bytes())
        .map_err(|_| "无法写入重启凭据")?;
    Ok(credential)
}

pub fn consume_restart_password(path: &Path) -> Result<String, String> {
    read_password_file_inner(path, true)
}

pub fn read_password_file(path: &Path) -> Result<String, String> {
    read_password_file_inner(path, false)
}

fn read_password_file_inner(path: &Path, consume: bool) -> Result<String, String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).map_err(|_| "无法读取密码文件")?;
    let metadata = file.metadata().map_err(|_| "无法检查密码文件")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("密码文件必须是普通文件".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("密码文件必须属于运行账号，且仅该账号可读写（chmod 600）".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("密码文件不能是链接".into());
        }
    }
    let mut bytes = Vec::new();
    file.take(RESTART_PASSWORD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取密码文件")?;
    if consume {
        std::fs::remove_file(path).map_err(|_| "无法清除重启凭据")?;
    }
    if bytes.is_empty() || bytes.len() > RESTART_PASSWORD_LIMIT {
        return Err("密码文件为空或过长".into());
    }
    let mut password = String::from_utf8(bytes).map_err(|_| "密码文件必须使用 UTF-8 编码")?;
    // Text editors commonly append one newline; internal handoffs preserve every byte.
    if !consume && password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    if password.trim().is_empty() {
        return Err("密码文件不能只含空白字符".into());
    }
    Ok(password)
}

pub fn restart_spec() -> Result<RestartSpec, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_word = resolve_restart_executable_word(&cwd, &exe);
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // Authentication is installation-local durable state. Never resurrect startup credentials.
    let credential: Option<RestartCredential> = None;
    let args = restart_args(
        &args,
        credential
            .as_ref()
            .map(|credential| credential.path.as_path()),
    );
    Ok(RestartSpec {
        cwd,
        exe: exe_word,
        args,
        credential,
    })
}

/// Relaunch without a password in argv, script text, or the child environment.
pub fn restart_command_line() -> Result<RestartCommand, String> {
    let spec = restart_spec()?;
    let mut parts = vec![
        "cd".to_string(),
        shell_word(&spec.cwd.to_string_lossy()),
        "&&".to_string(),
        "env -u BILISTREAM_PASSWORD".to_string(),
    ];
    parts.extend(restart_environment_words(|name| std::env::var(name).ok()));
    parts.push(shell_word(&spec.exe));
    parts.extend(spec.args.iter().map(|arg| shell_word(arg)));
    Ok(RestartCommand {
        text: parts.join(" "),
        credential: spec.credential,
    })
}

fn restart_environment_words(read: impl Fn(&str) -> Option<String>) -> Vec<String> {
    // A screen command runs in its original shell, whose environment can differ
    // from the process being replaced. Preserve supported startup overrides;
    // the password itself is resolved from the installation store.
    [
        "BILISTREAM_DATA_DIR",
        "BILISTREAM_KEY_FILE",
        "BILISTREAM_BIND",
        "BILISTREAM_PORT",
        "BILISTREAM_FFMPEG_LOG_LEVEL",
        "BILISTREAM_CLUSTER_TOKEN_FILE",
        "BILISTREAM_SCREEN_SESSION",
        "XDG_CONFIG_HOME",
    ]
    .into_iter()
    .filter_map(|name| read(name).map(|value| shell_word(&format!("{name}={value}"))))
    .collect()
}

pub fn windows_restart_bat() -> Result<RestartCommand, String> {
    let spec = restart_spec()?;
    let mut start = format!("start \"\" {}", cmd_word(&spec.exe));
    for arg in &spec.args {
        start.push(' ');
        start.push_str(&cmd_word(arg));
    }
    Ok(RestartCommand {
        text: format!("@echo off\r\nset BILISTREAM_PASSWORD=\r\ntimeout /t 2 /nobreak >nul\r\ncd /d {}\r\n{}\r\ndel \"%~f0\"\r\n", cmd_word(&spec.cwd.to_string_lossy()), start),
        credential: spec.credential,
    })
}

pub fn resolve_restart_executable_word(cwd: &Path, current_exe: &Path) -> String {
    if let Some(name) = current_exe.file_name().and_then(|name| name.to_str()) {
        let installed = match name.trim_end_matches(" (deleted)") {
            "bilistream.old" => Some("bilistream"),
            "bilistream.exe.old" => Some("bilistream.exe"),
            "bilistream-tauri.old" => Some("bilistream-tauri"),
            "bilistream-tauri.exe.old" => Some("bilistream-tauri.exe"),
            _ => None,
        };
        if let Some(installed) = installed {
            return current_exe
                .with_file_name(installed)
                .to_string_lossy()
                .into_owned();
        }
    }
    let local_bin = cwd.join("bilistream");
    let local_bin_exists = local_bin.exists();
    let current_exe_display = current_exe.to_string_lossy();
    let running_deleted_binary = current_exe_display.ends_with(" (deleted)");

    if local_bin_exists {
        if running_deleted_binary {
            return "./bilistream".to_string();
        }
        if std::fs::canonicalize(&local_bin).ok().as_ref()
            == std::fs::canonicalize(current_exe).ok().as_ref()
        {
            return "./bilistream".to_string();
        }
    }

    if running_deleted_binary {
        return current_exe_display
            .strip_suffix(" (deleted)")
            .unwrap_or(&current_exe_display)
            .to_string();
    }

    current_exe_display.to_string()
}

pub fn shell_word(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn cmd_word(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn restart_args(original: &[String], credential: Option<&Path>) -> Vec<String> {
    let mut args = Vec::new();
    let mut skip_value = false;
    for arg in original {
        if skip_value {
            skip_value = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "--password" | "--password-file" | "--restart-password-file"
        ) {
            skip_value = true;
            continue;
        }
        if arg.starts_with("--password=")
            || arg.starts_with("--password-file=")
            || arg.starts_with("--restart-password-file=")
        {
            continue;
        }
        args.push(arg.clone());
    }
    if let Some(path) = credential {
        args.extend([
            "--restart-password-file".into(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_preserves_startup_paths_without_copying_passwords() {
        let words = restart_environment_words(|name| match name {
            "BILISTREAM_KEY_FILE" => Some("/private/key's file".into()),
            "BILISTREAM_DATA_DIR" => Some("/private/data".into()),
            "BILISTREAM_PORT" => Some("3151".into()),
            "BILISTREAM_PASSWORD" => panic!("password must not enter the restart command"),
            _ => None,
        });
        assert_eq!(
            words,
            [
                "'BILISTREAM_DATA_DIR=/private/data'",
                "'BILISTREAM_KEY_FILE=/private/key'\\''s file'",
                "'BILISTREAM_PORT=3151'",
            ]
        );
    }

    #[test]
    fn update_backup_paths_restart_the_installed_binary() {
        use std::path::Path;
        for (old, new) in [
            ("bilistream.old", "bilistream"),
            ("bilistream.old (deleted)", "bilistream"),
            ("bilistream-tauri.exe.old", "bilistream-tauri.exe"),
        ] {
            let path = Path::new("/app").join(old);
            assert_eq!(
                super::resolve_restart_executable_word(Path::new("/elsewhere"), &path),
                Path::new("/app").join(new).to_string_lossy()
            );
        }
    }

    #[test]
    fn restart_arguments_strip_passwords_and_old_credential_paths() {
        let original = [
            "--webui",
            "--password",
            "secret",
            "--port",
            "3150",
            "--password=other",
            "--password-file",
            "/persistent/file",
            "--password-file=/other/persistent/file",
            "--restart-password-file",
            "/old/file",
            "--restart-password-file=/other/old/file",
        ];
        let original = original.map(String::from);
        let args = restart_args(&original, Some(Path::new("/private/auth-file.tmp")));
        assert_eq!(
            args,
            [
                "--webui",
                "--port",
                "3150",
                "--restart-password-file",
                "/private/auth-file.tmp"
            ]
        );
        assert_eq!(restart_args(&original, None), ["--webui", "--port", "3150"]);
    }

    #[test]
    fn credentials_are_private_consumed_once_and_cleaned_on_failed_handoff() {
        let directory = std::env::temp_dir().join(format!(
            "bilistream-restart-auth-test-{}",
            std::process::id()
        ));
        let credential = write_restart_password(&directory, "synthetic-secret").unwrap();
        let path = credential.path.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(consume_restart_password(&path).is_err());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let link = directory.join("symlink");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(consume_restart_password(&link).is_err());
            std::fs::remove_file(link).unwrap();
        }
        assert_eq!(read_password_file(&path).unwrap(), "synthetic-secret");
        assert!(path.exists());
        assert_eq!(consume_restart_password(&path).unwrap(), "synthetic-secret");
        assert!(!path.exists());
        assert!(consume_restart_password(&path).is_err());
        let credential = write_restart_password(&directory, " secret with spaces \r\n").unwrap();
        assert_eq!(
            read_password_file(&credential.path).unwrap(),
            " secret with spaces "
        );
        assert_eq!(
            consume_restart_password(&credential.path).unwrap(),
            " secret with spaces \r\n"
        );
        let credential = write_restart_password(&directory, " \n").unwrap();
        assert!(read_password_file(&credential.path).is_err());
        assert!(credential.path.exists());
        assert!(consume_restart_password(&credential.path).is_err());
        assert!(!credential.path.exists());
        let credential = write_restart_password(&directory, "synthetic-other-secret").unwrap();
        let path = credential.path.clone();
        drop(credential);
        assert!(!path.exists());
        std::fs::remove_dir(directory).unwrap();
    }
}
