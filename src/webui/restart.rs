use std::path::{Path, PathBuf};

pub struct RestartSpec {
    pub cwd: PathBuf,
    pub exe: String,
    pub args: Vec<String>,
}

pub fn restart_spec() -> Result<RestartSpec, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_word = resolve_restart_executable_word(&cwd, &exe);
    let mut args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    ensure_password_arg(&mut args, crate::webui::listen_password());
    Ok(RestartSpec {
        cwd,
        exe: exe_word,
        args,
    })
}

/// Shell command used to relaunch this process, including `--password` when set.
pub fn restart_command_line() -> Result<String, String> {
    let spec = restart_spec()?;
    let mut parts = vec![
        "cd".to_string(),
        shell_word(&spec.cwd.to_string_lossy()),
        "&&".to_string(),
        shell_word(&spec.exe),
    ];
    parts.extend(spec.args.iter().map(|arg| shell_word(arg)));
    Ok(parts.join(" "))
}

pub fn windows_restart_bat() -> Result<String, String> {
    let spec = restart_spec()?;
    let mut start = format!("start \"\" {}", cmd_word(&spec.exe));
    for arg in &spec.args {
        start.push(' ');
        start.push_str(&cmd_word(arg));
    }
    Ok(format!(
        "@echo off\r\ntimeout /t 2 /nobreak >nul\r\ncd /d {}\r\n{}\r\ndel \"%~f0\"\r\n",
        cmd_word(&spec.cwd.to_string_lossy()),
        start
    ))
}

pub fn resolve_restart_executable_word(cwd: &Path, current_exe: &Path) -> String {
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

fn args_have_password_flag(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--password" || arg.starts_with("--password="))
}

fn ensure_password_arg(args: &mut Vec<String>, password: Option<&str>) {
    let Some(password) = password.filter(|value| !value.is_empty()) else {
        return;
    };
    if args_have_password_flag(args) {
        return;
    }
    args.push("--password".to_string());
    args.push(password.to_string());
}

#[cfg(test)]
mod tests {
    use super::{args_have_password_flag, ensure_password_arg};

    #[test]
    fn adds_password_when_missing() {
        let mut args = vec!["webui".to_string()];
        ensure_password_arg(&mut args, Some("secret"));
        assert_eq!(args, ["webui", "--password", "secret"]);
    }

    #[test]
    fn keeps_existing_password_flag() {
        let mut args = vec!["--password".to_string(), "already".to_string()];
        ensure_password_arg(&mut args, Some("secret"));
        assert_eq!(args, ["--password", "already"]);
    }

    #[test]
    fn keeps_equals_password_flag() {
        let mut args = vec!["--password=already".to_string()];
        ensure_password_arg(&mut args, Some("secret"));
        assert_eq!(args, ["--password=already"]);
        assert!(args_have_password_flag(&args));
    }

    #[test]
    fn skips_when_no_password_configured() {
        let mut args = vec!["webui".to_string()];
        ensure_password_arg(&mut args, None);
        assert_eq!(args, ["webui"]);
    }
}
