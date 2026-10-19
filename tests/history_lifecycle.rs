//! Real process-death coverage for the production history startup and writer.
//! The fixture drives SessionGuard directly: source selection, FFmpeg and the
//! monitor loop belong to the separate relay acceptance test.

#![cfg(unix)]

use bilistream::history::{self, SessionGuard};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHILD_MODE: &str = "BILISTREAM_HISTORY_LIFECYCLE_CHILD";

#[tokio::test]
#[ignore = "self-reexec fixture for the SIGKILL lifecycle regression"]
async fn recorder_process() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    assert!(matches!(mode.as_str(), "running" | "reconnecting"));
    bilistream::install_crypto_provider();
    let cfg = bilistream::config::load_config().await.unwrap();
    assert_eq!(cfg.bililive.room, 0);
    assert!(!cfg.history.bili_stop_summary);
    history::start().await;
    bilistream::events::seal();
    assert!(history::recording_enabled());
    let guard = SessionGuard::start(&cfg, "YT", "isolated-source", "local fixture", None);
    guard.run_started(1);
    if mode == "reconnecting" {
        guard.run_exited_at(bilistream::storage::now(), 1, Some(1), false, true);
    }
    // No stats and no graceful flush. Only the writer's real timer can keep
    // the open session/run recent before the parent sends SIGKILL.
    std::future::pending::<()>().await;
    drop(guard);
}

struct Directory(PathBuf);

impl Directory {
    fn new(mode: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bilistream-history-lifecycle-{mode}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = bilistream::storage::Store::open(root.join("data"), root.join("key")).unwrap();
        store
            .write(
                "config.json",
                json!({
                    "auto_cover": false,
                    "enable_anti_collision": false,
                    "interval": 15,
                    "bililive": {
                        "room": 0,
                        "enable_danmaku_command": false,
                        "bili_rtmp_url": "",
                        "bili_rtmp_key": ""
                    },
                    "youtube": {"enable_monitor": false},
                    "twitch": {"enable_monitor": false},
                    "enable_lol_monitor": false,
                    "anti_collision_list": {},
                    "history": {"enabled": true, "sample_secs": 10, "bili_stop_summary": false}
                }),
            )
            .unwrap();
        // Release the installation lock before either recorder process opens it.
        drop(store);
        Self(root)
    }

    fn history_path(&self) -> PathBuf {
        self.0.join("data/history.sqlite")
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Recorder(Child);

impl Recorder {
    fn start(directory: &Directory, mode: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "recorder_process", "--ignored", "--nocapture"])
                .env(CHILD_MODE, mode)
                .env("BILISTREAM_DATA_DIR", directory.0.join("data"))
                .env("BILISTREAM_KEY_FILE", directory.0.join("key"))
                .current_dir(&directory.0)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }

    async fn ready(&mut self, path: &Path, expected_sessions: i64) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                self.0.try_wait().unwrap().is_none(),
                "recorder exited early"
            );
            if let Ok(snapshot) = snapshot(path, None) {
                if snapshot.sessions == expected_sessions {
                    return snapshot;
                }
            }
            assert!(
                Instant::now() < deadline,
                "recorder did not commit its session"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn kill(&mut self) -> i64 {
        self.0.kill().unwrap();
        assert_eq!(self.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        bilistream::storage::now() as i64
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
struct Snapshot {
    sessions: i64,
    id: i64,
    boot: i64,
    started: i64,
    seen: i64,
    ended: Option<i64>,
    reason: Option<String>,
    run_seen: i64,
    run_ended: Option<i64>,
    samples: i64,
}

fn snapshot(path: &Path, id: Option<i64>) -> rusqlite::Result<Snapshot> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.query_row(
        "SELECT (SELECT count(*) FROM sessions),s.id,s.boot_id,s.started_at,s.last_seen_at,
                s.ended_at,s.end_reason,r.last_seen_at,r.ended_at,(SELECT count(*) FROM sample_frames)
         FROM sessions s JOIN runs r ON r.session_id=s.id
         WHERE (?1 IS NULL OR s.id=?1) ORDER BY s.started_at DESC,s.id DESC LIMIT 1",
        params![id],
        |row| {
            Ok(Snapshot {
                sessions: row.get(0)?,
                id: row.get(1)?,
                boot: row.get(2)?,
                started: row.get(3)?,
                seen: row.get(4)?,
                ended: row.get(5)?,
                reason: row.get(6)?,
                run_seen: row.get(7)?,
                run_ended: row.get(8)?,
                samples: row.get(9)?,
            })
        },
    )
}

async fn assert_crash_recovery(mode: &str) {
    let directory = Directory::new(mode);
    let path = directory.history_path();
    let mut first = Recorder::start(&directory, mode);
    let initial = first.ready(&path, 1).await;
    assert_eq!(initial.ended, None);
    assert_eq!(initial.samples, 0);
    // Kill at a fixed elapsed time, not just after observing a heartbeat.
    // A 30-second heartbeat leaves >15 seconds unrecorded and must fail.
    tokio::time::sleep(Duration::from_secs(21)).await;
    let killed_at = first.kill();
    let crashed = snapshot(&path, Some(initial.id)).unwrap();
    assert!(killed_at - initial.started >= 20);
    assert_eq!(crashed.samples, 0, "no sample may hide a stale heartbeat");
    assert_eq!(
        crashed.ended, None,
        "SIGKILL bypasses the SessionGuard drop"
    );
    assert!(
        (0..=15).contains(&(killed_at - crashed.seen)),
        "{mode}: crash lost more than 15 seconds: killed={killed_at}, {crashed:?}"
    );
    if mode == "running" {
        assert_eq!(crashed.run_ended, None);
        assert!((0..=15).contains(&(killed_at - crashed.run_seen)));
    } else {
        assert_eq!(crashed.run_ended, initial.run_ended);
        assert!(crashed.run_ended.is_some());
    }

    let mut second = Recorder::start(&directory, "running");
    let new = second.ready(&path, 2).await;
    let recovered = snapshot(&path, Some(initial.id)).unwrap();
    assert_eq!(recovered.reason.as_deref(), Some("process_restart"));
    assert_eq!(recovered.ended, Some(crashed.seen));
    assert_eq!(recovered.run_ended, Some(crashed.run_seen));
    assert_eq!(
        recovered.seen, crashed.seen,
        "downtime must not extend the old session"
    );
    assert_ne!(new.boot, initial.boot);
    assert_ne!(new.id, initial.id);
    assert!(new.started >= killed_at);
    assert_eq!(new.ended, None);
    assert_eq!(new.run_ended, None);
    assert_eq!(new.samples, 0);
}

#[tokio::test]
async fn sigkill_reopen_keeps_sampleless_sessions_within_fifteen_seconds() {
    tokio::join!(
        assert_crash_recovery("running"),
        assert_crash_recovery("reconnecting")
    );
}
