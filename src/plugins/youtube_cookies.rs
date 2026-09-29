//! A managed Cookie jar with serialized, private-file yt-dlp round trips.
use crate::storage::{self, paths, Store};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

pub const MAX_COOKIE_BYTES: usize = 1024 * 1024;
const NAME: &str = "cookies.txt";

#[derive(Debug, Serialize)]
pub struct CookieSummary {
    pub count: usize,
}
#[derive(Serialize)]
pub struct CookieStatus {
    pub configured: bool,
    pub count: usize,
    pub revision: u64,
    pub updated_at: u64,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Cookie 文件格式无效，请导出 Netscape 格式",
    )
}

pub fn validate_netscape(input: &str) -> io::Result<CookieSummary> {
    if input.len() > MAX_COOKIE_BYTES || input.contains('\0') {
        return Err(invalid());
    }
    if !input.is_empty()
        && !input.lines().next().is_some_and(|line| {
            let line = line.trim_end_matches('\r');
            line == "# Netscape HTTP Cookie File" || line == "# HTTP Cookie File"
        })
    {
        return Err(invalid());
    }
    let mut count = 0;
    let mut keys = HashSet::new();
    for line in input.lines() {
        let line = line.trim_end_matches('\r');
        let line = if let Some(value) = line.strip_prefix("#HttpOnly_") {
            value
        } else {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            line
        };
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 7
            || parts[0].is_empty()
            || parts[0]
                .chars()
                .any(|c| c.is_whitespace() || c == '/' || c == ':')
            || !matches!(parts[1], "TRUE" | "FALSE")
            || !parts[2].starts_with('/')
            || !matches!(parts[3], "TRUE" | "FALSE")
            || (!parts[4].is_empty() && parts[4].parse::<u64>().is_err())
            || parts[5..]
                .iter()
                .any(|part| part.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        if !keys.insert((parts[0], parts[2], parts[5])) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cookie 文件包含重复记录",
            ));
        }
        count += 1;
    }
    Ok(CookieSummary { count })
}

fn status_from(store: &Store) -> io::Result<CookieStatus> {
    let Some(doc) = store.read(NAME)? else {
        return Ok(CookieStatus {
            configured: false,
            count: 0,
            revision: 0,
            updated_at: 0,
        });
    };
    let text = doc.value.as_str().ok_or_else(invalid)?;
    let count = validate_netscape(text)?.count;
    Ok(CookieStatus {
        configured: count != 0,
        count,
        revision: doc.revision,
        updated_at: doc.updated_at,
    })
}

pub fn status() -> io::Result<CookieStatus> {
    status_from(storage::global()?.as_ref())
}

pub async fn import(input: String, expected_revision: u64) -> io::Result<CookieStatus> {
    if validate_netscape(&input)?.count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "文件中没有 Cookie；清除登录请使用清除按钮",
        ));
    }
    let store = storage::global()?;
    tokio::task::spawn_blocking(move || {
        store.compare_exchange(NAME, expected_revision, Value::String(input))?;
        status_from(&store)
    })
    .await
    .map_err(io::Error::other)?
}

pub async fn clear(expected_revision: u64) -> io::Result<CookieStatus> {
    let store = storage::global()?;
    tokio::task::spawn_blocking(move || {
        store.compare_exchange(NAME, expected_revision, Value::String(String::new()))?;
        status_from(&store)
    })
    .await
    .map_err(io::Error::other)?
}

fn queue(store: &Store) -> Arc<AsyncMutex<()>> {
    static QUEUES: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let mut queues = QUEUES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Arc::clone(
        queues
            .entry(store.data_dir().to_path_buf())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

struct TemporaryJar {
    directory: PathBuf,
    path: PathBuf,
}
impl TemporaryJar {
    fn create(store: &Store, text: &str) -> io::Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| io::Error::other("无法创建私有 Cookie 文件"))?;
        let suffix: String = nonce.iter().map(|n| format!("{n:02x}")).collect();
        let directory = store
            .runtime_dir()
            .join(format!("yt-cookie-{}-{suffix}", std::process::id()));
        cleanup_stale(store.runtime_dir());
        paths::private_dir(&directory)?;
        let path = directory.join("cookies.txt");
        if let Err(error) = paths::write_private(&path, text.as_bytes()) {
            let _ = fs::remove_dir(&directory);
            return Err(error);
        }
        Ok(Self { directory, path })
    }
}
impl Drop for TemporaryJar {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.directory.join("child.pid"));
        let _ = fs::remove_dir(&self.directory);
    }
}

pub(crate) fn cleanup_stale(directory: &std::path::Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("yt-cookie-")
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        let Some(pid) = fs::read_to_string(path.join("child.pid"))
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        #[cfg(unix)]
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 }
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        #[cfg(windows)]
        let alive = windows_process_alive(pid);
        if !alive {
            let _ = fs::remove_file(path.join("cookies.txt"));
            let _ = fs::remove_file(path.join("child.pid"));
            let _ = fs::remove_dir(path);
        }
    }
}

#[cfg(windows)]
fn windows_process_alive(pid: u32) -> bool {
    use winapi::um::{
        handleapi::CloseHandle, processthreadsapi::OpenProcess, synchapi::WaitForSingleObject,
        winbase::WAIT_OBJECT_0, winnt::SYNCHRONIZE,
    };
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        // Invalid process ID means it exited; access denied is not evidence.
        return io::Error::last_os_error().raw_os_error() != Some(87);
    }
    let state = unsafe { WaitForSingleObject(process, 0) };
    unsafe {
        CloseHandle(process);
    }
    state != WAIT_OBJECT_0
}

pub async fn run(command: Command, browser: Option<String>, limit: Duration) -> io::Result<Output> {
    run_with_store(storage::global()?, command, browser, limit).await
}

async fn run_with_store(
    store: Arc<Store>,
    mut command: Command,
    browser: Option<String>,
    limit: Duration,
) -> io::Result<Output> {
    let deadline = tokio::time::Instant::now() + limit;
    let (mut sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = async {
            let queue = queue(&store);
            let _guard = tokio::select! {
                biased;
                _ = sender.closed() => return Err(io::Error::new(io::ErrorKind::Interrupted, "Cookie 查询已取消")),
                result = tokio::time::timeout_at(deadline, queue.lock()) => result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Cookie 查询等待超时"))?,
            };
            let mut jar = None;
            let mut expected = 0;
            // Ignore external yt-dlp config; it could override the managed jar.
            command.args(["--ignore-config", "--no-cookies", "--no-cookies-from-browser"]);
            if let Some(browser) = browser.filter(|s| !s.trim().is_empty()) {
                command.arg("--cookies-from-browser").arg(browser);
            } else if let Some(doc) = store.read(NAME)? {
                expected = doc.revision;
                let text = doc.value.as_str().ok_or_else(invalid)?;
                if validate_netscape(text)?.count > 0 {
                    let temporary = TemporaryJar::create(&store, text)?;
                    command.arg("--cookies").arg(&temporary.path);
                    jar = Some(temporary);
                }
            }
            let mut output = super::utils::command_output_cancellable(command, deadline, "yt-dlp", sender.closed(), jar.as_ref().map(|jar| jar.directory.join("child.pid")).as_deref()).await?;
            if let Some(jar) = jar {
                let current = store.read(NAME)?.map(|d| d.revision).unwrap_or(0);
                if current != expected { return Err(io::Error::new(io::ErrorKind::WouldBlock, "Cookie 已更新，请重试查询")); }
                if output.status.success() {
                    let metadata = fs::metadata(&jar.path)?;
                    if metadata.len() > MAX_COOKIE_BYTES as u64 { return Err(invalid()); }
                    let updated = fs::read_to_string(&jar.path)?;
                    if updated.is_empty() || !updated.ends_with('\n') { return Err(invalid()); }
                    validate_netscape(&updated)?;
                    let store = Arc::clone(&store);
                    tokio::task::spawn_blocking(move || store.compare_exchange(NAME, expected, Value::String(updated))).await.map_err(io::Error::other)??;
                }
                drop(jar);
            }
            output.stderr = safe_stderr(&output.stderr);
            Ok(output)
        }.await;
        let _ = sender.send(result);
    });
    receiver
        .await
        .map_err(|_| io::Error::other("Cookie 查询已停止"))?
}

fn safe_stderr(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    if !text.contains("ERROR") {
        return Vec::new();
    }
    // Preserve only the schedule information used by the monitor, never raw
    // cookie lines, authenticated URLs or upstream diagnostic payloads.
    if let Ok(pattern) = regex::Regex::new(r"(?i)(\d+)\s+(minutes?|hours?|days?)") {
        if let Some(m) = pattern.captures(&text) {
            return format!(
                "ERROR: [youtube] This live event will begin in {} {}",
                &m[1], &m[2]
            )
            .into_bytes();
        }
    }
    b"ERROR: [youtube] stream unavailable".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    const JAR: &str = "# Netscape HTTP Cookie File\n#HttpOnly_.example.invalid\tTRUE\t/\tTRUE\t0\tsession\tsynthetic\n";
    #[test]
    fn validates_http_only_session_and_rejects_bad_rows_without_echo() {
        assert_eq!(validate_netscape(JAR).unwrap().count, 1);
        assert!(validate_netscape("bad\tsecret-value")
            .unwrap_err()
            .to_string()
            .find("secret-value")
            .is_none());
        assert!(validate_netscape(&format!("{JAR}{}", JAR.lines().last().unwrap())).is_err());
        assert_eq!(
            validate_netscape("# Netscape HTTP Cookie File\n")
                .unwrap()
                .count,
            0
        );
    }
    #[test]
    fn stderr_never_returns_secret_cookie_lines() {
        assert_eq!(
            safe_stderr(b"WARNING: skipping cookie file entry: secret-value\nERROR: broken"),
            b"ERROR: [youtube] stream unavailable"
        );
        assert_eq!(
            safe_stderr(b"ERROR: [youtube] scheduled in 12 minutes secret"),
            b"ERROR: [youtube] This live event will begin in 12 minutes"
        );
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    const INITIAL: &str =
        "# Netscape HTTP Cookie File\n.example.invalid\tTRUE\t/\tTRUE\t0\tvalue\t1\n";
    const SCRIPT: &str = r#"
import pathlib,sys,time
mode,marker,release=sys.argv[1:4]
path=pathlib.Path(sys.argv[sys.argv.index('--cookies')+1])
if mode in ('wait','sleep'):
    pathlib.Path(marker).write_text('started')
    if mode=='sleep': time.sleep(60)
    else:
        while not pathlib.Path(release).exists(): time.sleep(.01)
if mode=='truncate': path.write_text('')
elif mode=='bad': path.write_text('# Netscape HTTP Cookie File\ninvalid-secret-row\n')
elif mode=='delete': path.write_text('# Netscape HTTP Cookie File\n')
else:
    count=int(path.read_text().splitlines()[-1].split('\t')[-1])
    path.write_text('# Netscape HTTP Cookie File\n.example.invalid\tTRUE\t/\tTRUE\t0\tvalue\t'+str(count+1)+'\n')
if mode=='failure':
    print('ERROR: synthetic-secret-row',file=sys.stderr)
    sys.exit(1)
print('synthetic-result')
"#;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce).unwrap();
            let name: String = nonce.iter().map(|n| format!("{n:02x}")).collect();
            let root = std::env::temp_dir().join(format!("bilistream-cookie-test-{name}"));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn store(&self) -> Arc<Store> {
            let store = Store::open(self.0.join("data"), self.0.join("keys/master"), None).unwrap();
            store.write(NAME, json!(INITIAL)).unwrap();
            store
        }
        fn command(&self, mode: &str) -> Command {
            let mut command = Command::new("python3");
            command
                .arg("-c")
                .arg(SCRIPT)
                .arg(mode)
                .arg(self.0.join("started"))
                .arg(self.0.join("release"));
            command
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn wait_for(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    fn clean(store: &Store) {
        assert_eq!(fs::read_dir(store.runtime_dir()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn concurrent_queries_observe_previous_cookie_writeback_and_cleanup() {
        let dir = Directory::new();
        let store = dir.store();
        let first = run_with_store(
            Arc::clone(&store),
            dir.command("increment"),
            None,
            Duration::from_secs(5),
        );
        let second = run_with_store(
            Arc::clone(&store),
            dir.command("increment"),
            None,
            Duration::from_secs(5),
        );
        let (a, b) = tokio::join!(first, second);
        assert!(a.unwrap().status.success());
        assert!(b.unwrap().status.success());
        assert!(store
            .read(NAME)
            .unwrap()
            .unwrap()
            .value
            .as_str()
            .unwrap()
            .ends_with("\t3\n"));
        clean(&store);
    }

    #[tokio::test]
    async fn rejected_partial_and_failed_output_preserve_original_jar() {
        let dir = Directory::new();
        let store = dir.store();
        for mode in ["truncate", "bad", "failure"] {
            let result = run_with_store(
                Arc::clone(&store),
                dir.command(mode),
                None,
                Duration::from_secs(5),
            )
            .await;
            if mode == "failure" {
                let output = result.unwrap();
                assert!(!output.status.success());
                assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-row"));
            } else {
                assert!(result.is_err());
            }
            assert_eq!(store.read(NAME).unwrap().unwrap().value, INITIAL);
            clean(&store);
        }
    }

    #[tokio::test]
    async fn successful_cookie_deletion_is_not_resurrected() {
        let dir = Directory::new();
        let store = dir.store();
        run_with_store(
            Arc::clone(&store),
            dir.command("delete"),
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!status_from(&store).unwrap().configured);
        clean(&store);
    }

    #[tokio::test]
    async fn clear_during_query_rejects_old_output_and_writeback() {
        let dir = Directory::new();
        let store = dir.store();
        let copy = Arc::clone(&store);
        let command = dir.command("wait");
        let query = tokio::spawn(async move {
            run_with_store(copy, command, None, Duration::from_secs(5)).await
        });
        wait_for(&dir.0.join("started")).await;
        let revision = store.revision(NAME).unwrap().unwrap();
        store.compare_exchange(NAME, revision, json!("")).unwrap();
        fs::write(dir.0.join("release"), b"ready").unwrap();
        assert_eq!(
            query.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!status_from(&store).unwrap().configured);
        clean(&store);
    }

    #[tokio::test]
    async fn cancelled_query_reaps_child_before_removing_cookie_file() {
        let dir = Directory::new();
        let store = dir.store();
        let copy = Arc::clone(&store);
        let command = dir.command("sleep");
        let query = tokio::spawn(async move {
            run_with_store(copy, command, None, Duration::from_secs(10)).await
        });
        wait_for(&dir.0.join("started")).await;
        let job = fs::read_dir(store.runtime_dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let pid: i32 = fs::read_to_string(job.join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        query.abort();
        assert!(query.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while job.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert_eq!(store.read(NAME).unwrap().unwrap().value, INITIAL);
        clean(&store);
    }

    #[tokio::test]
    async fn timed_out_query_keeps_data_and_cleans_runtime_directory() {
        let dir = Directory::new();
        let store = dir.store();
        let result = run_with_store(
            Arc::clone(&store),
            dir.command("sleep"),
            None,
            Duration::from_millis(150),
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(store.read(NAME).unwrap().unwrap().value, INITIAL);
        clean(&store);
    }
}
